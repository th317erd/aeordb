//! Bounded runtime primitives for the v4 physical mark phase.
//!
//! This module owns only the fully reserved dense bitmap. Workspace,
//! checkpoint, traversal, and destructive consumers land in later P4-3 units.

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::kv_pages::MAX_ENTRIES_PER_PAGE;
pub use crate::engine::kv_snapshot::CapturedKvSlotPositionV1 as MarkSlotPositionV1;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

#[derive(Debug, Error)]
pub enum MarkBitmapErrorV1 {
  #[error("mark bitmap geometry is invalid: {0}")]
  Geometry(&'static str),
  #[error("mark bitmap operation was canceled")]
  Canceled,
  #[error("mark bitmap position is outside the captured layout")]
  Position,
  #[error("mark bitmap memory admission failed: {0}")]
  Memory(#[source] MemoryCoordinatorError),
  #[error("mark bitmap allocation failed: {0}")]
  Allocation(String),
}

impl MarkBitmapErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Geometry(_) => "mark_bitmap_geometry",
      Self::Canceled => "mark_bitmap_cancelled",
      Self::Position => "mark_bitmap_position",
      Self::Memory(_) => "mark_bitmap_memory",
      Self::Allocation(_) => "mark_bitmap_allocation",
    }
  }
}

/// Dense liveness bitmap addressed by the captured KV bucket and slot.
///
/// The full byte geometry is admitted before allocation and remains reserved
/// until the bitmap drops. A bit is only a hint that the key occupying the
/// captured slot was visited; the slot key remains identity authority.
pub struct DenseMarkBitmapV1 {
  bucket_count: u64,
  slots_per_bucket: u32,
  bit_count: u64,
  marked_count: u64,
  bytes: Vec<u8>,
  cancellation: CancellationToken,
  _memory: MemoryReservation,
}

impl std::fmt::Debug for DenseMarkBitmapV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("DenseMarkBitmapV1")
      .field("bucket_count", &self.bucket_count)
      .field("slots_per_bucket", &self.slots_per_bucket)
      .field("bit_count", &self.bit_count)
      .field("marked_count", &self.marked_count)
      .field("byte_count", &self.bytes.len())
      .finish_non_exhaustive()
  }
}

impl DenseMarkBitmapV1 {
  pub fn new(
    bucket_count: u64,
    slots_per_bucket: u32,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, MarkBitmapErrorV1> {
    if bucket_count == 0 {
      return Err(MarkBitmapErrorV1::Geometry("bucket count is zero"));
    }
    if slots_per_bucket != MAX_ENTRIES_PER_PAGE as u32 {
      return Err(MarkBitmapErrorV1::Geometry("slot count does not match the captured KV page contract"));
    }
    let bit_count =
      bucket_count.checked_mul(u64::from(slots_per_bucket)).ok_or(MarkBitmapErrorV1::Geometry("bitmap bit count overflow"))?;
    let byte_count_u64 = bit_count.checked_add(7).ok_or(MarkBitmapErrorV1::Geometry("bitmap byte count overflow"))? / 8;
    let byte_count = usize::try_from(byte_count_u64).map_err(|_| MarkBitmapErrorV1::Geometry("bitmap does not fit address space"))?;
    if cancellation.is_cancelled() {
      return Err(MarkBitmapErrorV1::Canceled);
    }

    let reservation =
      memory.reserve(MemoryOwner::GarbageCollection, byte_count_u64, AdmissionClass::Maintenance).map_err(MarkBitmapErrorV1::Memory)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(byte_count).map_err(|error| MarkBitmapErrorV1::Allocation(error.to_string()))?;
    bytes.resize(byte_count, 0);
    if cancellation.is_cancelled() {
      return Err(MarkBitmapErrorV1::Canceled);
    }

    Ok(Self { bucket_count, slots_per_bucket, bit_count, marked_count: 0, bytes, cancellation, _memory: reservation })
  }

  pub const fn bucket_count(&self) -> u64 {
    self.bucket_count
  }

  pub const fn slots_per_bucket(&self) -> u32 {
    self.slots_per_bucket
  }

  pub const fn bit_count(&self) -> u64 {
    self.bit_count
  }

  pub fn byte_count(&self) -> usize {
    self.bytes.len()
  }

  pub const fn marked_count(&self) -> u64 {
    self.marked_count
  }

  pub fn bytes(&self) -> &[u8] {
    &self.bytes
  }

  pub fn mark(&mut self, position: MarkSlotPositionV1) -> Result<bool, MarkBitmapErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(MarkBitmapErrorV1::Canceled);
    }
    let (byte_index, mask) = self.position(position)?;
    if self.bytes[byte_index] & mask != 0 {
      return Ok(false);
    }
    self.bytes[byte_index] |= mask;
    self.marked_count = self.marked_count.checked_add(1).ok_or(MarkBitmapErrorV1::Geometry("marked count overflow"))?;
    Ok(true)
  }

  pub fn is_marked(&self, position: MarkSlotPositionV1) -> Result<bool, MarkBitmapErrorV1> {
    let (byte_index, mask) = self.position(position)?;
    Ok(self.bytes[byte_index] & mask != 0)
  }

  fn position(&self, position: MarkSlotPositionV1) -> Result<(usize, u8), MarkBitmapErrorV1> {
    if position.bucket_index >= self.bucket_count || position.slot_index >= self.slots_per_bucket {
      return Err(MarkBitmapErrorV1::Position);
    }
    let bit_index = position
      .bucket_index
      .checked_mul(u64::from(self.slots_per_bucket))
      .and_then(|base| base.checked_add(u64::from(position.slot_index)))
      .filter(|index| *index < self.bit_count)
      .ok_or(MarkBitmapErrorV1::Position)?;
    let byte_index = usize::try_from(bit_index / 8).map_err(|_| MarkBitmapErrorV1::Position)?;
    let mask = 1u8 << (bit_index % 8);
    Ok((byte_index, mask))
  }
}
