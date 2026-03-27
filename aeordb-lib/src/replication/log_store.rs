use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io;
use std::ops::RangeBounds;
use std::sync::{Arc, Mutex};

use openraft::entry::RaftEntry;
use openraft::storage::{IOFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::type_config::alias::{LogIdOf, VoteOf};
use openraft::RaftTypeConfig;

use super::TypeConfig;

/// Shared in-memory state backing both the log store and its reader.
///
/// Protected by a Mutex so readers and the store can coexist safely.
#[derive(Debug, Default)]
pub struct LogStoreInner {
  vote: Option<VoteOf<TypeConfig>>,
  log: BTreeMap<u64, <TypeConfig as RaftTypeConfig>::Entry>,
  last_purged_log_id: Option<LogIdOf<TypeConfig>>,
}

/// In-memory Raft log store.
///
/// Sufficient for prototyping single-node consensus. A file-backed
/// implementation will replace this once the architecture is proven.
#[derive(Debug, Clone, Default)]
pub struct InMemoryLogStore {
  inner: Arc<Mutex<LogStoreInner>>,
}

impl InMemoryLogStore {
  pub fn new() -> Self {
    Self {
      inner: Arc::new(Mutex::new(LogStoreInner::default())),
    }
  }
}

/// Reader half -- clones the same Arc so it sees the same data.
#[derive(Debug, Clone)]
pub struct InMemoryLogReader {
  inner: Arc<Mutex<LogStoreInner>>,
}

// ---------------------------------------------------------------------------
// RaftLogReader
// ---------------------------------------------------------------------------

impl RaftLogReader<TypeConfig> for InMemoryLogReader {
  async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
    &mut self,
    range: RB,
  ) -> Result<Vec<<TypeConfig as RaftTypeConfig>::Entry>, io::Error> {
    let guard = self.inner.lock().map_err(|error| {
      io::Error::other(format!("lock poisoned: {}", error))
    })?;
    let entries: Vec<_> = guard
      .log
      .range(range)
      .map(|(_index, entry)| entry.clone())
      .collect();
    Ok(entries)
  }

  async fn read_vote(&mut self) -> Result<Option<VoteOf<TypeConfig>>, io::Error> {
    let guard = self.inner.lock().map_err(|error| {
      io::Error::other(format!("lock poisoned: {}", error))
    })?;
    Ok(guard.vote)
  }
}

// ---------------------------------------------------------------------------
// RaftLogStorage
// ---------------------------------------------------------------------------

impl RaftLogStorage<TypeConfig> for InMemoryLogStore {
  type LogReader = InMemoryLogReader;

  async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, io::Error> {
    let guard = self.inner.lock().map_err(|error| {
      io::Error::other(format!("lock poisoned: {}", error))
    })?;

    let last_log_id = guard
      .log
      .iter()
      .next_back()
      .map(|(_index, entry)| entry.log_id())
      .or(guard.last_purged_log_id);

    Ok(LogState {
      last_purged_log_id: guard.last_purged_log_id,
      last_log_id,
    })
  }

  async fn get_log_reader(&mut self) -> Self::LogReader {
    InMemoryLogReader {
      inner: Arc::clone(&self.inner),
    }
  }

  async fn save_vote(&mut self, vote: &VoteOf<TypeConfig>) -> Result<(), io::Error> {
    let mut guard = self.inner.lock().map_err(|error| {
      io::Error::other(format!("lock poisoned: {}", error))
    })?;
    guard.vote = Some(*vote);
    Ok(())
  }

  async fn append<I>(&mut self, entries: I, callback: IOFlushed<TypeConfig>) -> Result<(), io::Error>
  where
    I: IntoIterator<Item = <TypeConfig as RaftTypeConfig>::Entry> + Send,
    I::IntoIter: Send,
  {
    let mut guard = self.inner.lock().map_err(|error| {
      io::Error::other(format!("lock poisoned: {}", error))
    })?;

    for entry in entries {
      let index = entry.log_id().index;
      guard.log.insert(index, entry);
    }

    // In-memory store: data is "persisted" immediately.
    callback.io_completed(Ok(()));

    Ok(())
  }

  async fn truncate_after(&mut self, last_log_id: Option<LogIdOf<TypeConfig>>) -> Result<(), io::Error> {
    let mut guard = self.inner.lock().map_err(|error| {
      io::Error::other(format!("lock poisoned: {}", error))
    })?;

    match last_log_id {
      Some(log_id) => {
        // Remove all entries with index > log_id.index
        let keys_to_remove: Vec<u64> = guard
          .log
          .range((log_id.index + 1)..)
          .map(|(key, _)| *key)
          .collect();
        for key in keys_to_remove {
          guard.log.remove(&key);
        }
      }
      None => {
        // Truncate everything
        guard.log.clear();
      }
    }

    Ok(())
  }

  async fn purge(&mut self, log_id: LogIdOf<TypeConfig>) -> Result<(), io::Error> {
    let mut guard = self.inner.lock().map_err(|error| {
      io::Error::other(format!("lock poisoned: {}", error))
    })?;

    // Remove all entries with index <= log_id.index
    let keys_to_remove: Vec<u64> = guard
      .log
      .range(..=log_id.index)
      .map(|(key, _)| *key)
      .collect();
    for key in keys_to_remove {
      guard.log.remove(&key);
    }

    guard.last_purged_log_id = Some(log_id);

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_log_store_creation() {
    let store = InMemoryLogStore::new();
    let guard = store.inner.lock().unwrap();
    assert!(guard.log.is_empty());
    assert!(guard.vote.is_none());
    assert!(guard.last_purged_log_id.is_none());
  }
}
