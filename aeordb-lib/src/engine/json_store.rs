//! Generic JSON-document store backed by a directory of `.json` files.
//!
//! The system store holds eight different entity types under
//! `/.aeordb-system/<kind>/<id>` — groups, users, api keys, magic links,
//! refresh tokens, plugins, peer configs, and peer sync states. Each had
//! its own near-identical `store_`, `get_`, `list_`, `delete_` functions.
//! This module collapses the common pattern into one generic so each entity
//! becomes a thin wrapper that names its prefix and ID strategy.
//!
//! For entities that need additional lookups (e.g. by-prefix for api keys,
//! by-username for users), the wrapper layers those on top of the JsonStore
//! base.

use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

/// A generic CRUDL store for JSON documents under a fixed system-path prefix.
///
/// The store is stateless — all methods take `engine` so the same `JsonStore`
/// constant can be used across the lifetime of the process. The prefix is a
/// `&'static str` so it can be declared as a `const`.
pub struct JsonStore<T> {
    prefix: &'static str,
    _phantom: PhantomData<T>,
}

/// Single-document variant of [`JsonStore`] for entities stored at one
/// fixed path rather than a directory of per-id files. Used for things like
/// the peer_configs list (one JSON array at `/.aeordb-system/cluster/peers`).
pub struct JsonDoc<T> {
    path: &'static str,
    _phantom: PhantomData<T>,
}

impl<T> JsonDoc<T>
where
    T: Serialize + DeserializeOwned,
{
    pub const fn new(path: &'static str) -> Self {
        Self {
            path,
            _phantom: PhantomData,
        }
    }

    pub fn put(
        &self,
        engine: &StorageEngine,
        ctx: &RequestContext,
        value: &T,
    ) -> EngineResult<()> {
        let ops = DirectoryOps::new(engine);
        let json = serde_json::to_vec(value)
            .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
        ops.store_file(ctx, self.path, &json, Some("application/json"))?;
        Ok(())
    }

    pub fn get(&self, engine: &StorageEngine) -> EngineResult<Option<T>> {
        let ops = DirectoryOps::new(engine);
        match ops.read_file(self.path) {
            Ok(data) => {
                let value: T = serde_json::from_slice(&data)
                    .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
                Ok(Some(value))
            }
            Err(EngineError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Convenience: `get` returning the supplied default when absent.
    pub fn get_or_default(&self, engine: &StorageEngine, default: T) -> EngineResult<T> {
        Ok(self.get(engine)?.unwrap_or(default))
    }
}

impl<T> JsonStore<T>
where
    T: Serialize + DeserializeOwned,
{
    /// Construct a new store rooted at `prefix` (e.g. `/.aeordb-system/groups`).
    /// `prefix` should NOT have a trailing slash.
    pub const fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            _phantom: PhantomData,
        }
    }

    fn path_for(&self, id: &str) -> String {
        format!("{}/{}", self.prefix, id)
    }

    /// Store a value at `<prefix>/<id>`, creating or overwriting.
    pub fn put(
        &self,
        engine: &StorageEngine,
        ctx: &RequestContext,
        id: &str,
        value: &T,
    ) -> EngineResult<()> {
        let ops = DirectoryOps::new(engine);
        let path = self.path_for(id);
        let json = serde_json::to_vec(value)
            .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
        ops.store_file(ctx, &path, &json, Some("application/json"))?;
        Ok(())
    }

    /// Retrieve the value at `<prefix>/<id>`. Returns `Ok(None)` if not found.
    pub fn get(&self, engine: &StorageEngine, id: &str) -> EngineResult<Option<T>> {
        let ops = DirectoryOps::new(engine);
        let path = self.path_for(id);
        match ops.read_file(&path) {
            Ok(data) => {
                let value: T = serde_json::from_slice(&data)
                    .map_err(|error| EngineError::JsonParseError(error.to_string()))?;
                Ok(Some(value))
            }
            Err(EngineError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// List every value under the prefix. Entries that fail to deserialize
    /// are silently skipped — they're treated as foreign content from a
    /// future schema rather than as fatal errors.
    pub fn list(&self, engine: &StorageEngine) -> EngineResult<Vec<T>> {
        let ops = DirectoryOps::new(engine);
        let entries = match ops.list_directory(self.prefix) {
            Ok(entries) => entries,
            Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut values = Vec::with_capacity(entries.len());
        for entry in &entries {
            let path = self.path_for(&entry.name);
            if let Ok(data) = ops.read_file(&path) {
                if let Ok(value) = serde_json::from_slice::<T>(&data) {
                    values.push(value);
                }
            }
        }
        Ok(values)
    }

    /// Delete the value at `<prefix>/<id>`. Returns `Ok(true)` if it existed,
    /// `Ok(false)` if not.
    pub fn delete(
        &self,
        engine: &StorageEngine,
        ctx: &RequestContext,
        id: &str,
    ) -> EngineResult<bool> {
        let ops = DirectoryOps::new(engine);
        let path = self.path_for(id);
        match ops.delete_file(ctx, &path) {
            Ok(()) => Ok(true),
            Err(EngineError::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }
}
