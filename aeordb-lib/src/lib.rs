//! # AeorDB
//!
//! A content-addressed file database with versioning, WASM plugins, and a query engine.
//!
//! ## Architecture
//!
//! - **StorageEngine** — append-only WAL + KV index with lock-free snapshot reads
//! - **DirectoryOps** — file CRUD with automatic directory propagation
//! - **QueryEngine** — indexed queries with trigram, phonetic, and scalar indexes
//! - **VersionManager** — snapshots, forks, export/import
//! - **PluginManager** — WASM plugin deployment and invocation
//! - **TaskQueue** — background task system with cron scheduling
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use aeordb::engine::{StorageEngine, DirectoryOps, RequestContext};
//!
//! let engine = StorageEngine::create("my.aeordb").unwrap();
//! let ctx = RequestContext::system();
//! let ops = DirectoryOps::new(&engine);
//! ops.store_file(&ctx, "/hello.txt", b"Hello World", Some("text/plain")).unwrap();
//! let content = ops.read_file("/hello.txt").unwrap();
//! assert_eq!(content, b"Hello World");
//! ```

pub mod auth;
pub mod engine;
pub mod logging;
pub mod metrics;
pub mod plugins;
pub mod server;
