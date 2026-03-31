use crate::engine::kv_store::{KVEntry, KVStore};

/// Manages a primary KV store with an optional buffer for writes during resize operations.
///
/// When the KV store needs to grow, a temporary buffer KVS captures new writes
/// while the primary is being expanded. After resize completes, the buffer is
/// merged back into the primary and discarded.
pub struct KVResizeManager {
  primary: KVStore,
  buffer: Option<KVStore>,
  is_resizing: bool,
}

impl KVResizeManager {
  pub fn new(primary: KVStore) -> Self {
    KVResizeManager {
      primary,
      buffer: None,
      is_resizing: false,
    }
  }

  /// Enter resize mode: create a small buffer KVS to capture writes
  /// while the primary is being grown.
  pub fn begin_resize(&mut self) {
    let hash_algo = self.primary.hash_algo();
    // Small NVT for the temporary buffer — 64 buckets is sufficient
    // since it only holds writes during the brief resize window.
    let buffer = KVStore::new(hash_algo, 64);
    self.buffer = Some(buffer);
    self.is_resizing = true;
  }

  /// Exit resize mode: merge all buffer entries into the primary,
  /// then discard the buffer.
  pub fn end_resize(&mut self) {
    if let Some(buffer) = self.buffer.take() {
      for entry in buffer.iter() {
        self.primary.insert(entry.clone());
      }
    }
    self.is_resizing = false;
  }

  pub fn is_resizing(&self) -> bool {
    self.is_resizing
  }

  /// Insert an entry. During resize, writes go to the buffer;
  /// otherwise they go directly to the primary.
  pub fn insert(&mut self, entry: KVEntry) {
    if self.is_resizing {
      if let Some(buffer) = self.buffer.as_mut() {
        buffer.insert(entry);
        return;
      }
    }
    self.primary.insert(entry);
  }

  /// Look up an entry by hash. During resize, check the buffer first
  /// (most recent writes), then fall through to the primary.
  pub fn get(&self, hash: &[u8]) -> Option<&KVEntry> {
    if self.is_resizing {
      if let Some(buffer) = self.buffer.as_ref() {
        if let Some(entry) = buffer.get(hash) {
          return Some(entry);
        }
      }
    }
    self.primary.get(hash)
  }

  /// Check whether a hash exists. During resize, checks buffer first,
  /// then falls through to primary.
  pub fn contains(&self, hash: &[u8]) -> bool {
    if self.is_resizing {
      if let Some(buffer) = self.buffer.as_ref() {
        if buffer.contains(hash) {
          return true;
        }
      }
    }
    self.primary.contains(hash)
  }

  pub fn primary(&self) -> &KVStore {
    &self.primary
  }

  pub fn primary_mut(&mut self) -> &mut KVStore {
    &mut self.primary
  }
}
