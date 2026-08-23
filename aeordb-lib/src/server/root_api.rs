//! Frozen root-selection and route-ownership contracts for namespace APIs.
//!
//! P7 wires handlers to these contracts in later slices. Keeping the model
//! storage-neutral here lets the reference target prove the complete public
//! surface before v4 service activation.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use serde::Serialize;

use crate::engine::HashAlgorithm;
use crate::engine::v4::read_view::{ReadViewRootMetadataV1, ReadableRootStateV1};

use super::responses::{ErrorResponse, error_codes};

const MAX_ROOT_ALIAS_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HttpMethodV1 {
  Get,
  Post,
  Put,
  Patch,
  Delete,
  Head,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootRouteClassV1 {
  SingleRootNamespace,
  MultiRoot,
  ContentStaging,
  HashRetrieval,
  OperationalSystem,
  Mutation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootSelectorLocationV1 {
  None,
  Query,
  JsonBody,
  RouteSpecific,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootResponseShapeV1 {
  None,
  JsonEnvelope,
  Headers,
  RootSet,
  PerResultRoots,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorizationOwnerV1 {
  Public,
  Middleware,
  RootOnly,
  CurrentThenSelectedPath,
  Handler,
  PluginHost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadViewProofV1 {
  ResolvedReadView,
  MultiRootResolver,
  ContentTransport,
  MutationRejectsGenericRoot,
  NoNamespace,
  PluginHost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteRootOperationContractV1 {
  pub method: HttpMethodV1,
  pub class: RootRouteClassV1,
  pub selector: RootSelectorLocationV1,
  pub response: RootResponseShapeV1,
  pub authorization: AuthorizationOwnerV1,
  pub proof: ReadViewProofV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteRootRegistrationContractV1 {
  pub path: &'static str,
  pub operations: &'static [RouteRootOperationContractV1],
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RootSelectorFieldsV1 {
  pub root_hash: Option<String>,
  pub snapshot: Option<String>,
  pub version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestedRootSelectorV1 {
  CurrentHead,
  ExplicitRoot(Vec<u8>),
  Snapshot(String),
  VersionRoot(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootApiErrorV1 {
  InvalidRootHash,
  InvalidRootSelector,
  InvalidPagination,
  InvalidPositionCursor,
  PositionRootMismatch,
  PositionOrderMismatch,
  RootExpired,
  InvalidNamespaceRoot,
  HistoricalViewUnavailable,
  DatabaseCorruption,
}

impl RootApiErrorV1 {
  pub const fn code(self) -> &'static str {
    match self {
      Self::InvalidRootHash => error_codes::INVALID_ROOT_HASH,
      Self::InvalidRootSelector => error_codes::INVALID_ROOT_SELECTOR,
      Self::InvalidPagination => error_codes::INVALID_PAGINATION,
      Self::InvalidPositionCursor => error_codes::INVALID_POSITION_CURSOR,
      Self::PositionRootMismatch => error_codes::POSITION_ROOT_MISMATCH,
      Self::PositionOrderMismatch => error_codes::POSITION_ORDER_MISMATCH,
      Self::RootExpired => error_codes::ROOT_EXPIRED,
      Self::InvalidNamespaceRoot => error_codes::INVALID_NAMESPACE_ROOT,
      Self::HistoricalViewUnavailable => error_codes::HISTORICAL_VIEW_UNAVAILABLE,
      Self::DatabaseCorruption => error_codes::DATABASE_CORRUPTION,
    }
  }

  pub const fn status(self) -> StatusCode {
    match self {
      Self::InvalidRootHash
      | Self::InvalidRootSelector
      | Self::InvalidPagination
      | Self::InvalidPositionCursor
      | Self::PositionRootMismatch
      | Self::PositionOrderMismatch => StatusCode::BAD_REQUEST,
      Self::RootExpired => StatusCode::GONE,
      Self::InvalidNamespaceRoot => StatusCode::NOT_FOUND,
      Self::HistoricalViewUnavailable => StatusCode::SERVICE_UNAVAILABLE,
      Self::DatabaseCorruption => StatusCode::INTERNAL_SERVER_ERROR,
    }
  }

  pub const fn message(self) -> &'static str {
    match self {
      Self::InvalidRootHash => "Root hash is invalid",
      Self::InvalidRootSelector => "Root selectors are invalid or mutually exclusive",
      Self::InvalidPagination => "Pagination parameters are invalid or mutually exclusive",
      Self::InvalidPositionCursor => "Position cursor is invalid",
      Self::PositionRootMismatch => "Position cursor belongs to another root",
      Self::PositionOrderMismatch => "Position cursor belongs to another order",
      Self::RootExpired => "Root has expired",
      Self::InvalidNamespaceRoot => "Root is not an admitted namespace root",
      Self::HistoricalViewUnavailable => "Exact historical view is unavailable",
      Self::DatabaseCorruption => "Database authority is corrupt",
    }
  }
}

#[derive(Debug)]
pub struct RootErrorHttpResponseV1 {
  pub status: StatusCode,
  pub body: ErrorResponse,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RootResponseV1 {
  pub hash: String,
  pub state: &'static str,
  pub expires_at: Option<i64>,
}

pub fn parse_root_selector_v1(fields: &RootSelectorFieldsV1, algorithm: HashAlgorithm) -> Result<RequestedRootSelectorV1, RootApiErrorV1> {
  let present = usize::from(fields.root_hash.is_some()) + usize::from(fields.snapshot.is_some()) + usize::from(fields.version.is_some());
  if present > 1 {
    return Err(RootApiErrorV1::InvalidRootSelector);
  }
  if let Some(root_hash) = fields.root_hash.as_deref() {
    return decode_root_hash(root_hash, algorithm).map(RequestedRootSelectorV1::ExplicitRoot);
  }
  if let Some(snapshot) = fields.snapshot.as_deref() {
    validate_alias(snapshot)?;
    return Ok(RequestedRootSelectorV1::Snapshot(snapshot.to_string()));
  }
  if let Some(version) = fields.version.as_deref() {
    return decode_root_hash(version, algorithm).map(RequestedRootSelectorV1::VersionRoot);
  }
  Ok(RequestedRootSelectorV1::CurrentHead)
}

pub fn root_response_v1(metadata: &ReadViewRootMetadataV1, algorithm: HashAlgorithm) -> Result<RootResponseV1, RootApiErrorV1> {
  if metadata.hash.len() != algorithm.hash_length() || metadata.hash.iter().all(|byte| *byte == 0) {
    return Err(RootApiErrorV1::InvalidNamespaceRoot);
  }
  let (state, expected_expiry) = match metadata.state {
    ReadableRootStateV1::Live => ("live", None),
    ReadableRootStateV1::Retained => ("retained", None),
    ReadableRootStateV1::PendingDelete { expires_at_ms, .. } => ("pending_delete", Some(expires_at_ms)),
  };
  if metadata.expires_at_ms != expected_expiry {
    return Err(RootApiErrorV1::DatabaseCorruption);
  }
  Ok(RootResponseV1 { hash: hex::encode(&metadata.hash), state, expires_at: expected_expiry })
}

pub fn root_response_headers_v1(metadata: &ReadViewRootMetadataV1, algorithm: HashAlgorithm) -> Result<HeaderMap, RootApiErrorV1> {
  let root = root_response_v1(metadata, algorithm)?;
  let mut headers = HeaderMap::new();
  headers.insert("x-aeordb-root-hash", HeaderValue::from_str(&root.hash).map_err(|_| RootApiErrorV1::DatabaseCorruption)?);
  headers.insert("x-aeordb-root-state", HeaderValue::from_static(root.state));
  let expires = root.expires_at.map_or_else(String::new, |value| value.to_string());
  headers.insert("x-aeordb-root-expires-at", HeaderValue::from_str(&expires).map_err(|_| RootApiErrorV1::DatabaseCorruption)?);
  Ok(headers)
}

pub fn root_error_response_v1(error: RootApiErrorV1, conceal: bool) -> RootErrorHttpResponseV1 {
  if conceal {
    return RootErrorHttpResponseV1 {
      status: StatusCode::NOT_FOUND,
      body: ErrorResponse::new("Not found").with_code(error_codes::NOT_FOUND),
    };
  }
  RootErrorHttpResponseV1 { status: error.status(), body: ErrorResponse::new(error.message()).with_code(error.code()) }
}

fn decode_root_hash(value: &str, algorithm: HashAlgorithm) -> Result<Vec<u8>, RootApiErrorV1> {
  let bytes = hex::decode(value).map_err(|_| RootApiErrorV1::InvalidRootHash)?;
  if bytes.len() != algorithm.hash_length() || bytes.iter().all(|byte| *byte == 0) {
    return Err(RootApiErrorV1::InvalidRootHash);
  }
  Ok(bytes)
}

fn validate_alias(value: &str) -> Result<(), RootApiErrorV1> {
  if value.is_empty() || value.len() > MAX_ROOT_ALIAS_BYTES || value.chars().any(char::is_control) {
    return Err(RootApiErrorV1::InvalidRootSelector);
  }
  Ok(())
}

macro_rules! op {
  ($method:ident, $class:ident, $selector:ident, $response:ident, $authorization:ident, $proof:ident) => {
    RouteRootOperationContractV1 {
      method: HttpMethodV1::$method,
      class: RootRouteClassV1::$class,
      selector: RootSelectorLocationV1::$selector,
      response: RootResponseShapeV1::$response,
      authorization: AuthorizationOwnerV1::$authorization,
      proof: ReadViewProofV1::$proof,
    }
  };
}

macro_rules! registration {
  ($path:literal, $($operation:expr),+ $(,)?) => {
    RouteRootRegistrationContractV1 { path: $path, operations: &[$($operation),+] }
  };
}

static ROUTE_ROOT_CONTRACTS_V1: &[RouteRootRegistrationContractV1] = &[
  registration!("/blobs/{hex_hash}", op!(Get, HashRetrieval, Query, Headers, CurrentThenSelectedPath, ResolvedReadView)),
  registration!("/files/search", op!(Post, SingleRootNamespace, JsonBody, JsonEnvelope, CurrentThenSelectedPath, ResolvedReadView)),
  registration!("/files/query", op!(Post, SingleRootNamespace, JsonBody, JsonEnvelope, CurrentThenSelectedPath, ResolvedReadView)),
  registration!("/files/fetch", op!(Post, SingleRootNamespace, JsonBody, Headers, CurrentThenSelectedPath, ResolvedReadView)),
  registration!("/files/download", op!(Post, SingleRootNamespace, JsonBody, Headers, CurrentThenSelectedPath, ResolvedReadView)),
  registration!("/files/mkdir", op!(Post, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/files/copy", op!(Post, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/files/share", op!(Post, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot)),
  registration!(
    "/files/shares",
    op!(Get, OperationalSystem, None, None, Middleware, NoNamespace),
    op!(Delete, Mutation, Query, None, Middleware, MutationRejectsGenericRoot),
  ),
  registration!("/files/shared-with-me", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!("/files/share-link", op!(Post, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/files/share-links", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!("/files/share-links/{key_id}", op!(Delete, Mutation, Query, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/files/deleted", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!("/files/restore", op!(Post, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/files", op!(Get, SingleRootNamespace, Query, JsonEnvelope, CurrentThenSelectedPath, ResolvedReadView)),
  registration!("/files/", op!(Get, SingleRootNamespace, Query, JsonEnvelope, CurrentThenSelectedPath, ResolvedReadView)),
  registration!(
    "/files/{*path}",
    op!(Put, Mutation, Query, None, Middleware, MutationRejectsGenericRoot),
    op!(Get, SingleRootNamespace, Query, Headers, CurrentThenSelectedPath, ResolvedReadView),
    op!(Delete, Mutation, Query, None, Middleware, MutationRejectsGenericRoot),
    op!(Head, SingleRootNamespace, Query, Headers, CurrentThenSelectedPath, ResolvedReadView),
    op!(Patch, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot),
  ),
  registration!("/blobs/chunks/{hash}", op!(Put, ContentStaging, None, None, Middleware, ContentTransport)),
  registration!("/versions/import", op!(Post, Mutation, RouteSpecific, None, RootOnly, MutationRejectsGenericRoot)),
  registration!("/blobs/check", op!(Post, ContentStaging, None, None, Middleware, ContentTransport)),
  registration!("/blobs/commit", op!(Post, ContentStaging, None, None, Middleware, ContentTransport)),
  registration!(
    "/auth/keys",
    op!(Post, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot),
    op!(Get, OperationalSystem, None, None, Middleware, NoNamespace),
  ),
  registration!("/auth/keys/{key_id}", op!(Delete, Mutation, Query, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/auth/keys/users", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!(
    "/auth/keys/admin",
    op!(Post, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
  ),
  registration!(
    "/auth/keys/admin/{key_id}",
    op!(Delete, Mutation, Query, None, RootOnly, MutationRejectsGenericRoot),
    op!(Patch, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
  ),
  registration!("/system/metrics", op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace)),
  registration!("/system/stats", op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace)),
  registration!(
    "/system/users",
    op!(Post, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
  ),
  registration!(
    "/system/users/{user_id}",
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
    op!(Patch, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
    op!(Delete, Mutation, Query, None, RootOnly, MutationRejectsGenericRoot),
  ),
  registration!(
    "/system/groups",
    op!(Post, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
  ),
  registration!(
    "/system/groups/{name}",
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
    op!(Patch, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
    op!(Delete, Mutation, Query, None, RootOnly, MutationRejectsGenericRoot),
  ),
  registration!("/versions/export", op!(Post, MultiRoot, RouteSpecific, RootSet, RootOnly, MultiRootResolver)),
  registration!("/versions/diff", op!(Post, MultiRoot, JsonBody, RootSet, RootOnly, MultiRootResolver)),
  registration!("/versions/promote", op!(Post, Mutation, RouteSpecific, None, RootOnly, MutationRejectsGenericRoot)),
  registration!(
    "/system/email-config",
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
    op!(Put, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
  ),
  registration!("/system/email-test", op!(Post, OperationalSystem, JsonBody, None, RootOnly, NoNamespace)),
  registration!("/system/gc", op!(Post, OperationalSystem, JsonBody, None, RootOnly, NoNamespace)),
  registration!("/system/repair", op!(Post, OperationalSystem, JsonBody, None, RootOnly, NoNamespace)),
  registration!("/system/tasks", op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace)),
  registration!("/system/tasks/reindex", op!(Post, OperationalSystem, JsonBody, None, RootOnly, NoNamespace)),
  registration!("/system/tasks/gc", op!(Post, OperationalSystem, JsonBody, None, RootOnly, NoNamespace)),
  registration!("/system/tasks/cleanup", op!(Post, OperationalSystem, JsonBody, None, RootOnly, NoNamespace)),
  registration!(
    "/system/tasks/{id}",
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
    op!(Delete, OperationalSystem, Query, None, RootOnly, NoNamespace),
  ),
  registration!(
    "/system/cron",
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
    op!(Post, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
  ),
  registration!(
    "/system/cron/{id}",
    op!(Delete, Mutation, Query, None, RootOnly, MutationRejectsGenericRoot),
    op!(Patch, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
  ),
  registration!(
    "/system/runtime",
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
    op!(Put, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
    op!(Patch, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
  ),
  registration!(
    "/system/lifecycle",
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
    op!(Put, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
    op!(Patch, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
  ),
  registration!("/blobs/config", op!(Get, ContentStaging, None, None, Middleware, ContentTransport)),
  registration!("/system/events", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!("/events/me", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!(
    "/versions/snapshots",
    op!(Post, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot),
    op!(Get, OperationalSystem, None, None, Middleware, NoNamespace),
  ),
  registration!("/versions/restore", op!(Post, Mutation, RouteSpecific, None, Middleware, MutationRejectsGenericRoot)),
  registration!(
    "/versions/snapshots/{name}",
    op!(Delete, Mutation, Query, None, Middleware, MutationRejectsGenericRoot),
    op!(Patch, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot),
  ),
  registration!(
    "/versions/forks",
    op!(Post, Mutation, RouteSpecific, None, Middleware, MutationRejectsGenericRoot),
    op!(Get, OperationalSystem, None, None, Middleware, NoNamespace),
  ),
  registration!("/versions/forks/{name}/promote", op!(Post, Mutation, RouteSpecific, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/versions/forks/{name}", op!(Delete, Mutation, Query, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/versions/history/{*path}", op!(Get, MultiRoot, Query, PerResultRoots, CurrentThenSelectedPath, MultiRootResolver)),
  registration!("/versions/restore/{*path}", op!(Post, Mutation, RouteSpecific, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/sync/conflicts", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!("/sync/conflicts/{*path}", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!("/sync/resolve/{*path}", op!(Post, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/sync/dismiss/{*path}", op!(Post, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot)),
  registration!("/sync/status", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!(
    "/sync/peers",
    op!(Post, Mutation, JsonBody, None, RootOnly, MutationRejectsGenericRoot),
    op!(Get, OperationalSystem, None, None, RootOnly, NoNamespace),
  ),
  registration!("/sync/peers/{node_id}", op!(Delete, Mutation, Query, None, RootOnly, MutationRejectsGenericRoot)),
  registration!("/sync/trigger", op!(Post, OperationalSystem, JsonBody, None, RootOnly, NoNamespace)),
  registration!("/sync/join", op!(Post, OperationalSystem, JsonBody, None, RootOnly, NoNamespace)),
  registration!(
    "/links/{*path}",
    op!(Put, Mutation, JsonBody, None, Middleware, MutationRejectsGenericRoot),
    op!(Get, SingleRootNamespace, Query, JsonEnvelope, CurrentThenSelectedPath, ResolvedReadView),
    op!(Delete, Mutation, Query, None, Middleware, MutationRejectsGenericRoot),
  ),
  registration!(
    "/plugins/{name}",
    op!(Put, Mutation, Query, None, RootOnly, MutationRejectsGenericRoot),
    op!(Delete, Mutation, Query, None, RootOnly, MutationRejectsGenericRoot),
  ),
  registration!("/plugins/{name}/invoke", op!(Post, OperationalSystem, JsonBody, JsonEnvelope, PluginHost, PluginHost)),
  registration!("/plugins", op!(Get, OperationalSystem, None, None, Middleware, NoNamespace)),
  registration!("/system/health", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/auth/token", op!(Post, OperationalSystem, JsonBody, None, Public, NoNamespace)),
  registration!("/auth/magic-link", op!(Post, OperationalSystem, JsonBody, None, Public, NoNamespace)),
  registration!("/auth/magic-link/verify", op!(Get, OperationalSystem, Query, None, Public, NoNamespace)),
  registration!("/auth/refresh", op!(Post, OperationalSystem, JsonBody, None, Public, NoNamespace)),
  registration!("/", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/app.mjs", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/metrics.mjs", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/dashboard.mjs", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/dashboard.css", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/users.mjs", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/groups.mjs", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/files.mjs", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/snapshots.mjs", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/settings.mjs", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/shared/{*path}", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/aeor/{*path}", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/docs", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/docs/", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/docs/{*path}", op!(Get, OperationalSystem, None, None, Public, NoNamespace)),
  registration!("/sync/diff", op!(Post, MultiRoot, JsonBody, RootSet, Handler, MultiRootResolver)),
  registration!("/sync/chunks", op!(Post, ContentStaging, None, None, Handler, ContentTransport)),
];

pub fn route_root_contracts_v1() -> &'static [RouteRootRegistrationContractV1] {
  ROUTE_ROOT_CONTRACTS_V1
}
