//! Streamed effective candidates; this projection grants no publication permit.
use super::*;

#[derive(Clone, Copy, Debug)]
pub struct QuarantineEffectiveClosureLimitsV1 {
  pub maximum_support_artifacts: u64,
  /// Every input row and physical-identity comparison consumes one work unit.
  pub maximum_work: u64,
}

pub struct QuarantineEffectiveClosureRequestV1<'a> {
  pub manifest: &'a QuarantineManifestV1<'a>,
  pub directory: Option<&'a GcStateDirectoryV1<'a>>,
  pub lifecycle: &'a GcStateManifestV1<'a>,
  /// Ordered immutable delta bodies; their owner retains the buffer charges.
  pub delta_values: &'a [&'a [u8]],
  pub hash_algorithm: HashAlgorithm,
  pub limits: QuarantineEffectiveClosureLimitsV1,
}

#[derive(Debug)]
pub struct QuarantineEffectiveClosureSummaryV1 {
  pub closure: QuarantineClosureSummaryV1,
  pub candidate_count: u64,
  pub candidate_bytes: u64,
  pub input_rows: u64,
  pub comparisons: u64,
  pub work: u64,
}

#[derive(Debug, Error)]
pub enum QuarantineEffectiveClosureErrorV1 {
  #[error(transparent)]
  Closure(#[from] QuarantineClosureErrorV1),
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error(transparent)]
  Memory(#[from] MemoryCoordinatorError),
  #[error(transparent)]
  Allocation(#[from] std::collections::TryReserveError),
  #[error("effective quarantine candidates exceeded their work limit")]
  WorkLimit,
  #[error("effective quarantine candidate traversal has already failed")]
  Failed,
}

impl QuarantineEffectiveClosureErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Closure(source) => source.code(),
      Self::Format(source) => source.code(),
      Self::Memory(_) => "quarantine_effective_memory",
      Self::Allocation(_) => "quarantine_effective_allocation",
      Self::WorkLimit => "quarantine_effective_work",
      Self::Failed => "quarantine_effective_failed",
    }
  }
}

pub struct QuarantineEffectiveClosureV1<'a> {
  validator: Option<QuarantineClosureValidatorV1<'a>>,
  manifest: &'a QuarantineManifestV1<'a>,
  cursors: Vec<DeltaCursor<'a>>,
  heap: Vec<usize>,
  previous_base: Vec<u8>,
  algorithm: HashAlgorithm,
  cancellation: CancellationToken,
  memory: MemoryReservation,
  maximum_work: u64,
  input_rows: u64,
  comparisons: u64,
  work: u64,
  candidate_count: u64,
  failed: bool,
}

struct DeltaCursor<'a> {
  rows: CandidateDeltaRecordsV1<'a>,
  head: Option<CandidateDeltaRecordV1<'a>>,
}

impl<'a> QuarantineEffectiveClosureV1<'a> {
  pub fn new(
    request: QuarantineEffectiveClosureRequestV1<'a>,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, QuarantineEffectiveClosureErrorV1> {
    if request.limits.maximum_work == 0 {
      return Err(QuarantineClosureErrorV1::InvalidConfiguration("effective merge work limit must be positive").into());
    }
    if cancellation.is_cancelled() {
      return Err(QuarantineClosureErrorV1::Canceled.into());
    }
    if request.delta_values.len() != request.manifest.delta_count as usize || request.delta_values.len() > MAXIMUM_CANDIDATE_DELTAS {
      return Err(closure_error("quarantine_delta_count", "effective merge requires the complete bounded delta list").into());
    }
    let total_bytes = request
      .delta_values
      .iter()
      .try_fold(0u64, |total, value| total.checked_add(value.len() as u64).ok_or(QuarantineEffectiveClosureErrorV1::WorkLimit))?;
    if total_bytes > MAXIMUM_CANDIDATE_DELTA_BYTES {
      return Err(closure_error("quarantine_delta_bytes", "effective merge exceeds the frozen delta byte bound").into());
    }
    let allocation_bytes = (request.delta_values.len() * (std::mem::size_of::<DeltaCursor<'_>>() + std::mem::size_of::<usize>())
      + std::mem::size_of::<Self>()
      + 24
      + 2 * request.hash_algorithm.hash_length()
      + 512) as u64;
    let reservation = memory.reserve(MemoryOwner::GarbageCollection, allocation_bytes, AdmissionClass::Maintenance)?;
    // Admit declared rows before the existing codecs walk them. Their length
    // validation precedes row iteration, so a false small count cannot evade
    // this bound. CRC/framing work is separately bounded by the delta byte cap.
    let mut declared_rows = 0u64;
    for value in request.delta_values {
      reservation.check_admission()?;
      if cancellation.is_cancelled() {
        return Err(QuarantineClosureErrorV1::Canceled.into());
      }
      let envelope = decode_gc_artifact_envelope(value)?;
      if envelope.kind != GcArtifactKindV1::CandidateDelta {
        return Err(closure_error("candidate_delta_identity", "effective merge input is not a candidate delta").into());
      }
      declared_rows = declared_rows
        .checked_add(u64::from(u32_at(envelope.body, 8)?))
        .filter(|rows| *rows <= request.limits.maximum_work)
        .ok_or(QuarantineEffectiveClosureErrorV1::WorkLimit)?;
    }
    let validator = QuarantineClosureValidatorV1::new(
      request.manifest,
      request.directory,
      request.lifecycle,
      request.hash_algorithm,
      cancellation.clone(),
      QuarantineClosureLimitsV1 { maximum_support_artifacts: request.limits.maximum_support_artifacts },
      memory,
    )?;
    let mut result = Self {
      validator: Some(validator),
      manifest: request.manifest,
      cursors: Vec::new(),
      heap: Vec::new(),
      previous_base: Vec::new(),
      algorithm: request.hash_algorithm,
      cancellation,
      memory: reservation,
      maximum_work: request.limits.maximum_work,
      input_rows: 0,
      comparisons: 0,
      work: 0,
      candidate_count: 0,
      failed: false,
    };
    result.cursors.try_reserve_exact(request.delta_values.len())?;
    result.heap.try_reserve_exact(request.delta_values.len())?;
    result.previous_base.try_reserve_exact(24 + 2 * request.hash_algorithm.hash_length())?;
    for value in request.delta_values {
      result.preflight()?;
      result.validator.as_mut().ok_or(QuarantineEffectiveClosureErrorV1::Failed)?.observe_delta(value)?;
      let delta = decode_candidate_delta_v1(value, request.hash_algorithm)?;
      let index = result.cursors.len();
      result.cursors.push(DeltaCursor { rows: delta.records()?, head: None });
      result.advance(index)?;
      if result.cursors[index].head.is_some() {
        result.heap.push(index);
        let mut child = result.heap.len() - 1;
        while child > 0 {
          let parent = (child - 1) / 2;
          if result.compare_heads(child, parent)? != Ordering::Less {
            break;
          }
          result.heap.swap(child, parent);
          child = parent;
        }
      }
    }
    result.preflight()?;
    Ok(result)
  }

  pub fn observe_base_page<E: From<QuarantineEffectiveClosureErrorV1>>(
    &mut self,
    page: &GcStatePageV1<'_>,
    visitor: &mut impl FnMut(PhysicalQuarantineCandidateV1<'_>) -> Result<(), E>,
  ) -> Result<(), E> {
    let result = self.observe_page_inner(page, visitor);
    self.latch_observation(result)
  }

  pub fn observe_base_directory(&mut self, directory: &GcStateDirectoryV1<'_>) -> Result<(), QuarantineEffectiveClosureErrorV1> {
    let result = (|| {
      self.preflight()?;
      self.validator.as_mut().ok_or(QuarantineEffectiveClosureErrorV1::Failed)?.observe_base_directory(directory)?;
      self.preflight()
    })();
    self.latch_observation(result)
  }

  fn latch_observation<E>(&mut self, result: Result<(), E>) -> Result<(), E> {
    match result {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish<E: From<QuarantineEffectiveClosureErrorV1>>(
    mut self,
    visitor: &mut impl FnMut(PhysicalQuarantineCandidateV1<'_>) -> Result<(), E>,
  ) -> Result<QuarantineEffectiveClosureSummaryV1, E> {
    self.preflight()?;
    let closure =
      self.validator.take().ok_or(QuarantineEffectiveClosureErrorV1::Failed)?.finish().map_err(QuarantineEffectiveClosureErrorV1::from)?;
    while !self.heap.is_empty() {
      let row = self.next_effective_delta()?;
      if row.operation == CandidateDeltaOperationV1::Set {
        self.emit(row.candidate, visitor)?;
      }
    }
    let candidate_bytes = self
      .candidate_count
      .checked_mul((52 + 2 * self.algorithm.hash_length()) as u64)
      .ok_or(QuarantineEffectiveClosureErrorV1::WorkLimit)?;
    if self.candidate_count != self.manifest.candidate_count || candidate_bytes != self.manifest.candidate_bytes {
      return Err(
        QuarantineEffectiveClosureErrorV1::from(closure_error(
          "quarantine_effective_totals",
          "effective candidate rows do not close against the selected manifest totals",
        ))
        .into(),
      );
    }
    self.preflight()?;
    Ok(QuarantineEffectiveClosureSummaryV1 {
      closure,
      candidate_count: self.candidate_count,
      candidate_bytes,
      input_rows: self.input_rows,
      comparisons: self.comparisons,
      work: self.work,
    })
  }

  fn observe_page_inner<E: From<QuarantineEffectiveClosureErrorV1>>(
    &mut self,
    page: &GcStatePageV1<'_>,
    visitor: &mut impl FnMut(PhysicalQuarantineCandidateV1<'_>) -> Result<(), E>,
  ) -> Result<(), E> {
    self.preflight()?;
    self
      .work
      .checked_add(u64::from(page.record_count))
      .filter(|work| *work <= self.maximum_work)
      .ok_or(QuarantineEffectiveClosureErrorV1::WorkLimit)?;
    self
      .validator
      .as_mut()
      .ok_or(QuarantineEffectiveClosureErrorV1::Failed)?
      .observe_base_page(page)
      .map_err(QuarantineEffectiveClosureErrorV1::from)?;
    for row in quarantine_candidate_records_v1(page, self.algorithm).map_err(QuarantineEffectiveClosureErrorV1::from)? {
      self.charge(false)?;
      let row = row.map_err(QuarantineEffectiveClosureErrorV1::from)?;
      if !self.previous_base.is_empty() {
        self.charge(true)?;
        let previous = decode_physical_incarnation(&self.previous_base, self.algorithm).map_err(QuarantineEffectiveClosureErrorV1::from)?;
        if compare_physical_incarnations_v1(&previous, &row.incarnation) != Ordering::Less {
          return Err(
            QuarantineEffectiveClosureErrorV1::from(order_error(
              "quarantine_effective_base_order",
              "effective base candidates must be strictly ordered across all pages",
            ))
            .into(),
          );
        }
      }
      self.previous_base.resize(24 + 2 * self.algorithm.hash_length(), 0);
      encode_physical_incarnation_into(&mut self.previous_base, &row.incarnation, self.algorithm)
        .map_err(QuarantineEffectiveClosureErrorV1::from)?;
      let mut overridden = false;
      while let Some(candidate) = self.peek() {
        self.charge(true)?;
        match compare_physical_incarnations_v1(&candidate.candidate.incarnation, &row.incarnation) {
          Ordering::Greater => break,
          order => {
            let delta = self.next_effective_delta()?;
            if delta.operation == CandidateDeltaOperationV1::Set {
              self.emit(delta.candidate, visitor)?;
            }
            if order == Ordering::Equal {
              overridden = true;
              break;
            }
          }
        }
      }
      if !overridden {
        self.emit(row, visitor)?;
      }
    }
    self.preflight()?;
    Ok(())
  }

  fn preflight(&self) -> Result<(), QuarantineEffectiveClosureErrorV1> {
    if self.failed {
      return Err(QuarantineEffectiveClosureErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(QuarantineClosureErrorV1::Canceled.into());
    }
    self.memory.check_admission()?;
    Ok(())
  }

  fn charge(&mut self, comparison: bool) -> Result<(), QuarantineEffectiveClosureErrorV1> {
    self.preflight()?;
    self.work = self.work.checked_add(1).filter(|work| *work <= self.maximum_work).ok_or(QuarantineEffectiveClosureErrorV1::WorkLimit)?;
    if comparison {
      self.comparisons += 1;
    } else {
      self.input_rows += 1;
    }
    Ok(())
  }

  fn advance(&mut self, index: usize) -> Result<(), QuarantineEffectiveClosureErrorV1> {
    if self.cursors[index].rows.len() == 0 {
      self.cursors[index].head = None;
      return Ok(());
    }
    self.charge(false)?;
    self.cursors[index].head = Some(self.cursors[index].rows.next().ok_or(QuarantineEffectiveClosureErrorV1::Failed)??);
    Ok(())
  }

  fn peek(&self) -> Option<CandidateDeltaRecordV1<'a>> {
    self.heap.first().and_then(|index| self.cursors[*index].head)
  }

  fn compare_heads(&mut self, left: usize, right: usize) -> Result<Ordering, QuarantineEffectiveClosureErrorV1> {
    self.charge(true)?;
    let left = self.cursors[self.heap[left]].head.ok_or(QuarantineEffectiveClosureErrorV1::Failed)?;
    let right = self.cursors[self.heap[right]].head.ok_or(QuarantineEffectiveClosureErrorV1::Failed)?;
    Ok(compare_physical_incarnations_v1(&left.candidate.incarnation, &right.candidate.incarnation))
  }

  fn pop(&mut self) -> Result<(usize, CandidateDeltaRecordV1<'a>), QuarantineEffectiveClosureErrorV1> {
    let index = *self.heap.first().ok_or(QuarantineEffectiveClosureErrorV1::Failed)?;
    let row = self.cursors[index].head.ok_or(QuarantineEffectiveClosureErrorV1::Failed)?;
    self.advance(index)?;
    if self.cursors[index].head.is_none() {
      let last = self.heap.pop().ok_or(QuarantineEffectiveClosureErrorV1::Failed)?;
      if !self.heap.is_empty() {
        self.heap[0] = last;
      }
    }
    let mut parent = 0;
    while parent * 2 + 1 < self.heap.len() {
      let mut child = parent * 2 + 1;
      if child + 1 < self.heap.len() && self.compare_heads(child + 1, child)? == Ordering::Less {
        child += 1;
      }
      if self.compare_heads(child, parent)? != Ordering::Less {
        break;
      }
      self.heap.swap(child, parent);
      parent = child;
    }
    Ok((index, row))
  }

  fn next_effective_delta(&mut self) -> Result<CandidateDeltaRecordV1<'a>, QuarantineEffectiveClosureErrorV1> {
    let (mut latest, mut result) = self.pop()?;
    while let Some(next) = self.peek() {
      self.charge(true)?;
      if compare_physical_incarnations_v1(&next.candidate.incarnation, &result.candidate.incarnation) != Ordering::Equal {
        break;
      }
      let (index, row) = self.pop()?;
      if index > latest {
        latest = index;
        result = row;
      }
    }
    Ok(result)
  }

  fn emit<E: From<QuarantineEffectiveClosureErrorV1>>(
    &mut self,
    row: PhysicalQuarantineCandidateV1<'_>,
    visitor: &mut impl FnMut(PhysicalQuarantineCandidateV1<'_>) -> Result<(), E>,
  ) -> Result<(), E> {
    self.preflight()?;
    self.candidate_count = self.candidate_count.checked_add(1).ok_or(QuarantineEffectiveClosureErrorV1::WorkLimit)?;
    visitor(row)
  }
}
