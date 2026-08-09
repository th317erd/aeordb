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

use crate::engine::directory_ops::{BufferedFileTransform, DirectoryOps};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::namespace_mutation::NamespaceMutationKind;
use crate::engine::request_context::RequestContext;
use crate::engine::schema_version::JsonVersioned;
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

pub(crate) enum JsonStoreMutation<T, O> {
  Keep(O),
  Replace { value: T, output: O },
}

impl<T> JsonDoc<T>
where
  T: JsonVersioned,
{
  pub const fn new(path: &'static str) -> Self {
    Self { path, _phantom: PhantomData }
  }

  pub fn put(&self, engine: &StorageEngine, ctx: &RequestContext, value: &T) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let json = value.serialize_versioned();
    ops.store_file_buffered(ctx, self.path, &json, Some("application/json"))?;
    Ok(())
  }

  pub fn get(&self, engine: &StorageEngine) -> EngineResult<Option<T>> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file_buffered(self.path) {
      Ok(data) => Ok(Some(T::deserialize_versioned(&data)?)),
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }

  /// Retrieve this singleton only after its declared size passes the caller's
  /// allocation bound.
  pub fn get_bounded(&self, engine: &StorageEngine, maximum_bytes: u64) -> EngineResult<Option<T>> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file_buffered_bounded(self.path, maximum_bytes) {
      Ok(data) => Ok(Some(T::deserialize_versioned(&data)?)),
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }

  /// Convenience: `get` returning the supplied default when absent.
  pub fn get_or_default(&self, engine: &StorageEngine, default: T) -> EngineResult<T> {
    Ok(self.get(engine)?.unwrap_or(default))
  }

  /// Bounded variant of [`Self::get_or_default`].
  pub fn get_or_default_bounded(&self, engine: &StorageEngine, default: T, maximum_bytes: u64) -> EngineResult<T> {
    Ok(self.get_bounded(engine, maximum_bytes)?.unwrap_or(default))
  }

  /// Atomically read, decode, transform, and optionally replace this bounded
  /// singleton document while namespace authority is held.
  pub(crate) fn transform<O, F>(&self, engine: &StorageEngine, ctx: &RequestContext, maximum_bytes: u64, transform: F) -> EngineResult<O>
  where
    F: FnOnce(Option<T>) -> EngineResult<JsonStoreMutation<T, O>>,
  {
    let ops = DirectoryOps::new(engine);
    ops.transform_file_buffered(
      ctx,
      self.path,
      Some("application/json"),
      maximum_bytes,
      NamespaceMutationKind::SystemWrite,
      move |existing| {
        let current = existing.map(T::deserialize_versioned).transpose()?;
        match transform(current)? {
          JsonStoreMutation::Keep(output) => Ok(BufferedFileTransform::Keep(output)),
          JsonStoreMutation::Replace { value, output } => Ok(BufferedFileTransform::Replace { data: value.serialize_versioned(), output }),
        }
      },
    )
  }
}

impl<T> JsonStore<T>
where
  T: JsonVersioned,
{
  /// Construct a new store rooted at `prefix` (e.g. `/.aeordb-system/groups`).
  /// `prefix` should NOT have a trailing slash.
  pub const fn new(prefix: &'static str) -> Self {
    Self { prefix, _phantom: PhantomData }
  }

  fn path_for(&self, id: &str) -> String {
    format!("{}/{}", self.prefix, id)
  }

  /// Store a value at `<prefix>/<id>`, creating or overwriting.
  pub fn put(&self, engine: &StorageEngine, ctx: &RequestContext, id: &str, value: &T) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let path = self.path_for(id);
    let json = value.serialize_versioned();
    ops.store_file_buffered(ctx, &path, &json, Some("application/json"))?;
    Ok(())
  }

  /// Atomically read, decode, transform, and optionally replace one bounded
  /// versioned document under namespace authority.
  ///
  /// Callers must expose typed operations around this internal callback. The
  /// callback runs while namespace authority is held and must not perform I/O
  /// or re-enter the engine.
  pub(crate) fn transform<O, F>(
    &self,
    engine: &StorageEngine,
    ctx: &RequestContext,
    id: &str,
    maximum_bytes: u64,
    transform: F,
  ) -> EngineResult<O>
  where
    F: FnOnce(Option<T>) -> EngineResult<JsonStoreMutation<T, O>>,
  {
    let ops = DirectoryOps::new(engine);
    let path = self.path_for(id);
    ops.transform_file_buffered(ctx, &path, Some("application/json"), maximum_bytes, NamespaceMutationKind::SystemWrite, move |existing| {
      let current = existing.map(T::deserialize_versioned).transpose()?;
      match transform(current)? {
        JsonStoreMutation::Keep(output) => Ok(BufferedFileTransform::Keep(output)),
        JsonStoreMutation::Replace { value, output } => Ok(BufferedFileTransform::Replace { data: value.serialize_versioned(), output }),
      }
    })
  }

  /// Retrieve the value at `<prefix>/<id>`. Returns `Ok(None)` if not found.
  pub fn get(&self, engine: &StorageEngine, id: &str) -> EngineResult<Option<T>> {
    let ops = DirectoryOps::new(engine);
    let path = self.path_for(id);
    match ops.read_file_buffered(&path) {
      Ok(data) => Ok(Some(T::deserialize_versioned(&data)?)),
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }

  /// List every value under the prefix. This is authoritative system state,
  /// so unreadable or unsupported records fail the complete enumeration.
  pub fn list(&self, engine: &StorageEngine) -> EngineResult<Vec<T>> {
    let ops = DirectoryOps::new(engine);
    let entries = match ops.list_directory_strict(self.prefix) {
      Ok(entries) => entries,
      Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
      Err(error) => return Err(error),
    };
    let mut values = Vec::with_capacity(entries.len());
    for entry in &entries {
      let path = self.path_for(&entry.name);
      let data = ops.read_file_buffered(&path)?;
      values.push(T::deserialize_versioned(&data)?);
    }
    Ok(values)
  }

  /// Delete the value at `<prefix>/<id>`. Returns `Ok(true)` if it existed,
  /// `Ok(false)` if not.
  pub fn delete(&self, engine: &StorageEngine, ctx: &RequestContext, id: &str) -> EngineResult<bool> {
    let ops = DirectoryOps::new(engine);
    let path = self.path_for(id);
    match ops.delete_file(ctx, &path) {
      Ok(()) => Ok(true),
      Err(EngineError::NotFound(_)) => Ok(false),
      Err(error) => Err(error),
    }
  }
}
