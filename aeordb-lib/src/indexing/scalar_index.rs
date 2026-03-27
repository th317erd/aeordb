use super::offset_table::OffsetTable;
use super::scalar_mapping::ScalarMapping;

/// Statistics about a scalar index.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexStats {
  pub entry_count: u64,
  pub table_capacity: usize,
  pub utilization_percentage: f64,
}

/// The main index structure combining a scalar mapping with an offset table.
///
/// Values are mapped to scalars via the mapping function, then stored in
/// the offset table for efficient lookup.
pub struct ScalarIndex {
  mapping: Box<dyn ScalarMapping>,
  offset_table: OffsetTable,
  name: String,
  entry_count: u64,
}

impl ScalarIndex {
  /// Create a new scalar index with the given name, mapping, and initial capacity.
  pub fn new(name: String, mapping: Box<dyn ScalarMapping>, initial_capacity: usize) -> Self {
    Self {
      mapping,
      offset_table: OffsetTable::new(initial_capacity),
      name,
      entry_count: 0,
    }
  }

  /// Map a value to its scalar, then insert into the offset table.
  pub fn insert(&mut self, value: &[u8], location: u64) {
    let scalar = self.mapping.map_to_scalar(value);
    self.offset_table.insert(scalar, location);
    self.entry_count += 1;
  }

  /// Map a value to its scalar, then find an exact match in the offset table.
  /// Returns the data location if found at the exact scalar position.
  pub fn lookup_exact(&self, value: &[u8]) -> Option<u64> {
    let scalar = self.mapping.map_to_scalar(value);
    let entry = self.offset_table.lookup(scalar)?;

    // Only return if the scalar matches exactly (within floating-point epsilon)
    if (entry.scalar - scalar).abs() < f64::EPSILON {
      Some(entry.location)
    } else {
      None
    }
  }

  /// Range query: return all locations for values with scalars in [f(min_value), f(max_value)].
  pub fn lookup_range(&self, min_value: &[u8], max_value: &[u8]) -> Vec<u64> {
    let min_scalar = self.mapping.map_to_scalar(min_value);
    let max_scalar = self.mapping.map_to_scalar(max_value);
    self.offset_table
      .lookup_range(min_scalar, max_scalar)
      .iter()
      .map(|entry| entry.location)
      .collect()
  }

  /// Return all locations with scalar > f(value).
  pub fn lookup_greater_than(&self, value: &[u8]) -> Vec<u64> {
    let scalar = self.mapping.map_to_scalar(value);
    self.offset_table
      .entries_greater_than(scalar)
      .iter()
      .map(|entry| entry.location)
      .collect()
  }

  /// Return all locations with scalar < f(value).
  pub fn lookup_less_than(&self, value: &[u8]) -> Vec<u64> {
    let scalar = self.mapping.map_to_scalar(value);
    self.offset_table
      .entries_less_than(scalar)
      .iter()
      .map(|entry| entry.location)
      .collect()
  }

  /// Self-correcting write-back: update the entry at this value's scalar
  /// with the correct location.
  pub fn correct(&mut self, value: &[u8], correct_location: u64) {
    let scalar = self.mapping.map_to_scalar(value);
    self.offset_table.correct_entry(scalar, correct_location);
  }

  /// Remove an entry matching the value's scalar and the given location.
  pub fn remove(&mut self, value: &[u8], location: u64) -> bool {
    let scalar = self.mapping.map_to_scalar(value);
    let removed = self.offset_table.remove(scalar, location);
    if removed {
      self.entry_count -= 1;
    }
    removed
  }

  /// Return index statistics.
  pub fn stats(&self) -> IndexStats {
    IndexStats {
      entry_count: self.entry_count,
      table_capacity: self.offset_table.capacity(),
      utilization_percentage: self.offset_table.utilization() * 100.0,
    }
  }

  /// Get the index name.
  pub fn name(&self) -> &str {
    &self.name
  }

  /// Get a reference to the offset table (for testing/inspection).
  pub fn offset_table(&self) -> &OffsetTable {
    &self.offset_table
  }

  /// Get a mutable reference to the offset table.
  pub fn offset_table_mut(&mut self) -> &mut OffsetTable {
    &mut self.offset_table
  }
}
