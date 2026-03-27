/// A single entry in the offset table mapping a scalar position to a data location.
#[derive(Debug, Clone, PartialEq)]
pub struct OffsetEntry {
  /// The [0.0, 1.0] scalar position (or [-1.0, 1.0] for signed types).
  pub scalar: f64,
  /// Offset/pointer to the data location.
  pub location: u64,
  /// True if this entry might be stale after a resize operation.
  pub is_approximate: bool,
}

/// Ordered offset table mapping scalar positions to data locations.
///
/// Entries are maintained in sorted order by scalar value. The table supports
/// self-correcting reads: when a stale entry is detected, the caller can
/// write back the corrected location, healing the index over time.
pub struct OffsetTable {
  entries: Vec<OffsetEntry>,
  capacity: usize,
}

impl OffsetTable {
  /// Create a new offset table with the given capacity.
  pub fn new(capacity: usize) -> Self {
    Self {
      entries: Vec::with_capacity(capacity),
      capacity,
    }
  }

  /// Insert an entry, maintaining sorted order by scalar.
  /// If the table is at capacity, the insert still succeeds (the capacity
  /// is a soft limit used for utilization tracking).
  pub fn insert(&mut self, scalar: f64, location: u64) {
    let entry = OffsetEntry {
      scalar,
      location,
      is_approximate: false,
    };

    let position = self.entries
      .binary_search_by(|probe| probe.scalar.partial_cmp(&scalar).unwrap_or(std::cmp::Ordering::Equal))
      .unwrap_or_else(|position| position);

    self.entries.insert(position, entry);
  }

  /// Find the closest entry to the given scalar via binary search.
  pub fn lookup(&self, scalar: f64) -> Option<&OffsetEntry> {
    if self.entries.is_empty() {
      return None;
    }

    match self.entries.binary_search_by(|probe| {
      probe.scalar.partial_cmp(&scalar).unwrap_or(std::cmp::Ordering::Equal)
    }) {
      Ok(index) => Some(&self.entries[index]),
      Err(index) => {
        // Find the closest entry
        if index == 0 {
          Some(&self.entries[0])
        } else if index >= self.entries.len() {
          self.entries.last()
        } else {
          let distance_before = (self.entries[index - 1].scalar - scalar).abs();
          let distance_after = (self.entries[index].scalar - scalar).abs();
          if distance_before <= distance_after {
            Some(&self.entries[index - 1])
          } else {
            Some(&self.entries[index])
          }
        }
      }
    }
  }

  /// Return all entries in the given scalar range [min_scalar, max_scalar].
  pub fn lookup_range(&self, min_scalar: f64, max_scalar: f64) -> Vec<&OffsetEntry> {
    if self.entries.is_empty() {
      return Vec::new();
    }

    let start = match self.entries.binary_search_by(|probe| {
      probe.scalar.partial_cmp(&min_scalar).unwrap_or(std::cmp::Ordering::Equal)
    }) {
      Ok(index) => index,
      Err(index) => index,
    };

    let end = match self.entries.binary_search_by(|probe| {
      probe.scalar.partial_cmp(&max_scalar).unwrap_or(std::cmp::Ordering::Equal)
    }) {
      Ok(index) => index + 1,
      Err(index) => index,
    };

    self.entries[start..end].iter().collect()
  }

  /// Self-correcting write-back: update the entry closest to this scalar
  /// with the correct location, and mark it as no longer approximate.
  pub fn correct_entry(&mut self, scalar: f64, correct_location: u64) {
    if self.entries.is_empty() {
      return;
    }

    let index = match self.entries.binary_search_by(|probe| {
      probe.scalar.partial_cmp(&scalar).unwrap_or(std::cmp::Ordering::Equal)
    }) {
      Ok(index) => index,
      Err(index) => {
        if index == 0 {
          0
        } else if index >= self.entries.len() {
          self.entries.len() - 1
        } else {
          let distance_before = (self.entries[index - 1].scalar - scalar).abs();
          let distance_after = (self.entries[index].scalar - scalar).abs();
          if distance_before <= distance_after {
            index - 1
          } else {
            index
          }
        }
      }
    };

    self.entries[index].location = correct_location;
    self.entries[index].is_approximate = false;
  }

  /// Resize the table to a new capacity, marking all existing entries as approximate.
  pub fn resize(&mut self, new_capacity: usize) {
    self.capacity = new_capacity;
    for entry in &mut self.entries {
      entry.is_approximate = true;
    }
  }

  /// Remove an entry matching both scalar and location.
  /// Returns true if an entry was removed.
  pub fn remove(&mut self, scalar: f64, location: u64) -> bool {
    if let Some(position) = self.entries.iter().position(|entry| {
      (entry.scalar - scalar).abs() < f64::EPSILON && entry.location == location
    }) {
      self.entries.remove(position);
      return true;
    }
    false
  }

  /// Number of entries currently in the table.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Whether the table is empty.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Utilization as a fraction: entries / capacity.
  pub fn utilization(&self) -> f64 {
    if self.capacity == 0 {
      return 0.0;
    }
    self.entries.len() as f64 / self.capacity as f64
  }

  /// Current capacity.
  pub fn capacity(&self) -> usize {
    self.capacity
  }

  /// Return entries with `scalar > threshold`.
  pub fn entries_greater_than(&self, threshold: f64) -> Vec<&OffsetEntry> {
    let start = match self.entries.binary_search_by(|probe| {
      probe.scalar.partial_cmp(&threshold).unwrap_or(std::cmp::Ordering::Equal)
    }) {
      Ok(index) => index + 1,
      Err(index) => index,
    };

    self.entries[start..].iter().collect()
  }

  /// Return entries with `scalar < threshold`.
  pub fn entries_less_than(&self, threshold: f64) -> Vec<&OffsetEntry> {
    let end = match self.entries.binary_search_by(|probe| {
      probe.scalar.partial_cmp(&threshold).unwrap_or(std::cmp::Ordering::Equal)
    }) {
      Ok(index) => index,
      Err(index) => index,
    };

    self.entries[..end].iter().collect()
  }

  /// Check if any entries are marked approximate.
  pub fn has_approximate_entries(&self) -> bool {
    self.entries.iter().any(|entry| entry.is_approximate)
  }

  /// Get a reference to the raw entries (for testing/inspection).
  pub fn entries(&self) -> &[OffsetEntry] {
    &self.entries
  }
}
