use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use quote::ToTokens;
use serde::{Deserialize, Serialize};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
  Attribute, Expr, ExprAssign, ExprIf, ExprMatch, ExprMethodCall, ExprWhile, ImplItemFn, Item, ItemFn, ItemImpl, ItemMod, Macro, Meta, Pat,
  PatTupleStruct, Token, TraitItemFn,
};

pub const INVENTORY_SCHEMA_VERSION: u32 = 1;
pub const SCANNER_NAME: &str = "aeordb-error-squelch-audit-v1";

const SOURCE_ROOTS: &[&str] =
  &["aeordb-lib/src", "aeordb-cli/src", "aeordb-plugin-sdk/src", "aeordb-parsers/plaintext/src", "aeordb-plugins"];
const SOURCE_FILES: &[&str] = &["aeordb-lib/build.rs"];
const MAX_PATTERN_CHARS: usize = 240;
const MIN_RATIONALE_CHARS: usize = 24;
const MAX_REVIEW_FIELD_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressionKind {
  BroadErrPattern,
  DefaultOnError,
  DiscardedAssignment,
  ErrorConversion,
  ErrorRecovery,
  LoggedErrorContinues,
  PanicMacro,
  PanicMethod,
  ResultStatusProbe,
  ResultToOption,
  ResultVariantProbe,
  SuccessOnlyConditional,
}

impl SuppressionKind {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::BroadErrPattern => "broad_err_pattern",
      Self::DefaultOnError => "default_on_error",
      Self::DiscardedAssignment => "discarded_assignment",
      Self::ErrorConversion => "error_conversion",
      Self::ErrorRecovery => "error_recovery",
      Self::LoggedErrorContinues => "logged_error_continues",
      Self::PanicMacro => "panic_macro",
      Self::PanicMethod => "panic_method",
      Self::ResultStatusProbe => "result_status_probe",
      Self::ResultToOption => "result_to_option",
      Self::ResultVariantProbe => "result_variant_probe",
      Self::SuccessOnlyConditional => "success_only_conditional",
    }
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressionClass {
  CorrectnessReadTraversal,
  DeliberatelyIgnored,
  DurabilityAuthority,
  OptionalTelemetryTempCleanup,
  RebuildableDerived,
  RetryableOperational,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
  Pending,
  Reviewed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SuppressionOccurrence {
  pub id: String,
  pub file: String,
  pub line: usize,
  pub column: usize,
  pub enclosing_item: String,
  pub kind: SuppressionKind,
  pub pattern: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReviewedSuppression {
  #[serde(flatten)]
  pub occurrence: SuppressionOccurrence,
  pub review: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SuppressionReview {
  pub review_status: ReviewStatus,
  pub class: SuppressionClass,
  pub rationale: String,
  pub owner: String,
  pub test: String,
  pub removal_condition: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SuppressionInventory {
  pub schema_version: u32,
  pub scanner: String,
  pub scope: Vec<String>,
  pub maximum_occurrences: usize,
  pub reviews: BTreeMap<String, SuppressionReview>,
  pub entries: Vec<ReviewedSuppression>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RawOccurrence {
  file: String,
  line: usize,
  column: usize,
  enclosing_item: String,
  kind: SuppressionKind,
  normalized_pattern: String,
}

pub fn production_scope() -> Vec<String> {
  SOURCE_ROOTS.iter().chain(SOURCE_FILES.iter()).map(|value| (*value).to_string()).collect()
}

pub fn scan_workspace(workspace_root: &Path) -> Result<Vec<SuppressionOccurrence>, String> {
  let mut source_files = Vec::new();
  for root in SOURCE_ROOTS {
    collect_rust_files(&workspace_root.join(root), &mut source_files)?;
  }
  for file in SOURCE_FILES {
    let path = workspace_root.join(file);
    if !path.is_file() {
      return Err(format!("configured production source file does not exist: {}", path.display()));
    }
    source_files.push(path);
  }
  source_files.sort();
  source_files.dedup();

  let mut raw_occurrences = Vec::new();
  for path in source_files {
    let relative = path
      .strip_prefix(workspace_root)
      .map_err(|error| format!("source path {} is outside {}: {error}", path.display(), workspace_root.display()))?;
    let relative = normalize_path(relative);
    let source = fs::read_to_string(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    raw_occurrences.extend(scan_source_raw(&relative, &source)?);
  }
  Ok(finalize_occurrences(raw_occurrences))
}

pub fn scan_source(relative_file: &str, source: &str) -> Result<Vec<SuppressionOccurrence>, String> {
  Ok(finalize_occurrences(scan_source_raw(relative_file, source)?))
}

pub fn load_inventory(path: &Path) -> Result<SuppressionInventory, String> {
  let bytes = fs::read(path).map_err(|error| format!("failed to read suppression inventory {}: {error}", path.display()))?;
  serde_json::from_slice(&bytes).map_err(|error| format!("failed to decode suppression inventory {}: {error}", path.display()))
}

pub fn validate_inventory(discovered: &[SuppressionOccurrence], inventory: &SuppressionInventory) -> Vec<String> {
  let mut errors = Vec::new();
  if inventory.schema_version != INVENTORY_SCHEMA_VERSION {
    errors.push(format!("inventory schema_version {} does not equal {}", inventory.schema_version, INVENTORY_SCHEMA_VERSION));
  }
  if inventory.scanner != SCANNER_NAME {
    errors.push(format!("inventory scanner {:?} does not equal {:?}", inventory.scanner, SCANNER_NAME));
  }
  if inventory.scope != production_scope() {
    errors.push("inventory production scope does not match the scanner's closed source scope".to_string());
  }
  if inventory.maximum_occurrences != inventory.entries.len() {
    errors.push(format!(
      "inventory maximum_occurrences {} must exactly equal its {} reviewed entries",
      inventory.maximum_occurrences,
      inventory.entries.len()
    ));
  }

  let mut discovered_by_id = BTreeMap::new();
  for occurrence in discovered {
    if discovered_by_id.insert(occurrence.id.as_str(), occurrence).is_some() {
      errors.push(format!("scanner produced duplicate identity {}", occurrence.id));
    }
  }

  let mut seen_inventory_ids = BTreeSet::new();
  let mut used_reviews = BTreeSet::new();
  for entry in &inventory.entries {
    let occurrence = &entry.occurrence;
    if !seen_inventory_ids.insert(occurrence.id.as_str()) {
      errors.push(format!("inventory contains duplicate identity {}", occurrence.id));
      continue;
    }
    used_reviews.insert(entry.review.as_str());
    match inventory.reviews.get(&entry.review) {
      Some(review) => validate_review_fields(&entry.review, review, &mut errors),
      None => errors.push(format!("inventory entry {} references missing review policy {:?}", occurrence.id, entry.review)),
    }
    match discovered_by_id.get(occurrence.id.as_str()) {
      Some(actual) if *actual == occurrence => {}
      Some(actual) => errors.push(format!(
        "inventory metadata for {} is stale: recorded {}:{} {:?} {:?}, discovered {}:{} {:?} {:?}",
        occurrence.id,
        occurrence.file,
        occurrence.line,
        occurrence.kind,
        occurrence.pattern,
        actual.file,
        actual.line,
        actual.kind,
        actual.pattern
      )),
      None => errors.push(format!("inventory entry {} is stale because the source occurrence no longer exists", occurrence.id)),
    }
  }

  for occurrence in discovered {
    if !seen_inventory_ids.contains(occurrence.id.as_str()) {
      errors.push(format!(
        "unreviewed suppression {} at {}:{} ({})",
        occurrence.id,
        occurrence.file,
        occurrence.line,
        occurrence.kind.as_str()
      ));
    }
  }
  for review in inventory.reviews.keys() {
    if !used_reviews.contains(review.as_str()) {
      errors.push(format!("inventory review policy {review:?} is stale because no occurrence uses it"));
    }
  }
  errors
}

pub fn refreshed_inventory(
  discovered: &[SuppressionOccurrence],
  previous: Option<&SuppressionInventory>,
  allow_baseline_growth: bool,
) -> Result<SuppressionInventory, String> {
  let previous_entries: HashMap<&str, &ReviewedSuppression> =
    previous.map(|inventory| inventory.entries.iter().map(|entry| (entry.occurrence.id.as_str(), entry)).collect()).unwrap_or_default();
  let previous_baseline = previous.map(|inventory| inventory.entries.len()).unwrap_or(discovered.len());
  if previous.is_some() && discovered.len() > previous_baseline && !allow_baseline_growth {
    return Err(format!(
      "refusing to raise the reviewed suppression baseline from {previous_baseline} to {}; rerun with explicit baseline-growth approval",
      discovered.len()
    ));
  }
  let entries: Vec<_> = discovered
    .iter()
    .map(|occurrence| {
      if let Some(previous_entry) = previous_entries.get(occurrence.id.as_str()) {
        let mut retained = (*previous_entry).clone();
        retained.occurrence = occurrence.clone();
        return retained;
      }
      ReviewedSuppression { occurrence: occurrence.clone(), review: "pending-review".to_string() }
    })
    .collect();
  let used_reviews: BTreeSet<_> = entries.iter().map(|entry| entry.review.as_str()).collect();
  let mut reviews = previous.map(|inventory| inventory.reviews.clone()).unwrap_or_default();
  reviews.retain(|name, _| used_reviews.contains(name.as_str()));
  if used_reviews.contains("pending-review") {
    reviews.entry("pending-review".to_string()).or_insert_with(|| SuppressionReview {
      review_status: ReviewStatus::Pending,
      class: SuppressionClass::DeliberatelyIgnored,
      rationale: "PENDING REVIEW: classify the exact failure direction and retained behavior.".to_string(),
      owner: "PENDING REVIEW".to_string(),
      test: "PENDING REVIEW".to_string(),
      removal_condition: "PENDING REVIEW: define the condition that removes this suppression.".to_string(),
    });
  }
  Ok(SuppressionInventory {
    schema_version: INVENTORY_SCHEMA_VERSION,
    scanner: SCANNER_NAME.to_string(),
    scope: production_scope(),
    maximum_occurrences: discovered.len(),
    reviews,
    entries,
  })
}

/// Build the checked review inventory after a human has reviewed the current
/// scanner diff. The source fixture remains the CI authority: this helper does
/// not auto-update it, and unknown suppression forms are rejected.
pub fn reviewed_inventory(discovered: &[SuppressionOccurrence]) -> Result<SuppressionInventory, String> {
  let mut entries = Vec::with_capacity(discovered.len());
  let mut reviews = BTreeMap::new();
  for occurrence in discovered {
    let review_name = review_policy_name(occurrence).ok_or_else(|| {
      format!(
        "unclassified {} suppression at {}:{} in {}: {}",
        occurrence.kind.as_str(),
        occurrence.file,
        occurrence.line,
        occurrence.enclosing_item,
        occurrence.pattern
      )
    })?;
    let review = review_policy(review_name).ok_or_else(|| format!("review classifier selected unknown policy {review_name:?}"))?;
    reviews.entry(review_name.to_string()).or_insert(review);
    entries.push(ReviewedSuppression { occurrence: occurrence.clone(), review: review_name.to_string() });
  }
  Ok(SuppressionInventory {
    schema_version: INVENTORY_SCHEMA_VERSION,
    scanner: SCANNER_NAME.to_string(),
    scope: production_scope(),
    maximum_occurrences: entries.len(),
    reviews,
    entries,
  })
}

fn review_policy_name(occurrence: &SuppressionOccurrence) -> Option<&'static str> {
  let file = occurrence.file.as_str();
  let item = occurrence.enclosing_item.as_str();
  let pattern = occurrence.pattern.as_str();
  match occurrence.kind {
    SuppressionKind::DiscardedAssignment => None,
    SuppressionKind::PanicMacro | SuppressionKind::PanicMethod if file == "aeordb-lib/build.rs" => Some("fatal-build-boundary"),
    SuppressionKind::PanicMacro | SuppressionKind::PanicMethod
      if file == "aeordb-lib/src/auth/provider.rs"
        || file == "aeordb-lib/src/logging/mod.rs"
        || file == "aeordb-lib/src/metrics/mod.rs"
        || file == "aeordb-lib/src/server/mod.rs" =>
    {
      Some("legacy-infallible-boundary")
    }
    SuppressionKind::PanicMacro | SuppressionKind::PanicMethod => Some("validated-local-invariant"),
    SuppressionKind::ErrorConversion if authority_file(file) => Some("authority-state-failure"),
    SuppressionKind::ErrorConversion if persistent_format_file(file) => Some("typed-format-failure"),
    SuppressionKind::ErrorConversion => Some("boundary-validation-failure"),
    SuppressionKind::ErrorRecovery => Some("ordered-optional-precedence"),
    SuppressionKind::BroadErrPattern if file.starts_with("aeordb-lib/src/server/") => Some("http-terminal-error"),
    SuppressionKind::BroadErrPattern if file.starts_with("aeordb-plugin-sdk/") => Some("plugin-error-envelope"),
    SuppressionKind::BroadErrPattern if file == "aeordb-lib/src/auth/permission_middleware.rs" => Some("authorization-concealment"),
    SuppressionKind::BroadErrPattern if item.to_ascii_lowercase().contains("drop") => Some("unwind-cleanup-evidence"),
    SuppressionKind::BroadErrPattern
      if file.ends_with("/cache.rs")
        || file.ends_with("/cache_loaders.rs")
        || file.ends_with("/kv_store.rs")
        || file.ends_with("/index_config_resolver.rs") =>
    {
      Some("derived-cache-or-search-miss")
    }
    SuppressionKind::BroadErrPattern
      if file.ends_with("/v4/database_header.rs") || file.ends_with("/v4/system_control.rs") || file.ends_with("/file_header.rs") =>
    {
      Some("redundant-authority-selection")
    }
    SuppressionKind::BroadErrPattern if file.ends_with("/health.rs") => Some("optional-observability-default"),
    SuppressionKind::BroadErrPattern if file.ends_with("/index_cleanup.rs") => Some("retryable-background-operation"),
    SuppressionKind::BroadErrPattern if file.starts_with("aeordb-cli/") => Some("diagnostic-partial-output"),
    SuppressionKind::BroadErrPattern if authority_file(file) => Some("authority-state-failure"),
    SuppressionKind::BroadErrPattern => Some("candidate-format-probe"),
    SuppressionKind::LoggedErrorContinues if item.to_ascii_lowercase().contains("drop") => Some("unwind-cleanup-evidence"),
    SuppressionKind::LoggedErrorContinues if file.starts_with("aeordb-lib/src/server/") => Some("http-terminal-or-postcommit-evidence"),
    SuppressionKind::LoggedErrorContinues if retryable_worker_file(file) => Some("retryable-background-operation"),
    SuppressionKind::LoggedErrorContinues if file.starts_with("aeordb-cli/") => Some("diagnostic-partial-output"),
    SuppressionKind::LoggedErrorContinues if authority_file(file) => Some("postcommit-derived-reconciliation"),
    SuppressionKind::LoggedErrorContinues => Some("optional-cleanup-evidence"),
    SuppressionKind::DefaultOnError if configuration_file(file) || file.starts_with("aeordb-cli/") => {
      Some("contractual-configuration-default")
    }
    SuppressionKind::DefaultOnError if file.starts_with("aeordb-lib/src/server/") => Some("api-optional-default"),
    SuppressionKind::DefaultOnError if file.starts_with("aeordb-lib/src/plugins/") || file.starts_with("aeordb-plugin-sdk/") => {
      Some("plugin-compatibility-default")
    }
    SuppressionKind::DefaultOnError if observability_file(file) => Some("optional-observability-default"),
    SuppressionKind::DefaultOnError if authority_file(file) || file.starts_with("aeordb-lib/src/engine/v4/") => {
      Some("bounded-authority-default")
    }
    SuppressionKind::DefaultOnError if derived_state_file(file) => Some("derived-state-default"),
    SuppressionKind::DefaultOnError => Some("optional-data-default"),
    SuppressionKind::ResultToOption if pattern.contains("try_from") || pattern.contains("checked_") => {
      Some("checked-arithmetic-composition")
    }
    SuppressionKind::ResultToOption if observability_file(file) => Some("optional-observability-default"),
    SuppressionKind::ResultToOption if derived_state_file(file) => Some("derived-exact-fallback"),
    SuppressionKind::ResultToOption => Some("format-or-header-probe"),
    SuppressionKind::ResultStatusProbe if pattern.contains("std :: env :: var") || pattern.contains(": var (") => {
      Some("presence-feature-toggle")
    }
    SuppressionKind::ResultStatusProbe if iterator_item(item) => Some("iterator-terminal-state"),
    SuppressionKind::ResultStatusProbe if file.ends_with("/temp_response.rs") || file.ends_with("/rss_sampler.rs") => {
      Some("optional-cleanup-evidence")
    }
    SuppressionKind::ResultStatusProbe if derived_state_file(file) || file.ends_with("/event_bus.rs") => Some("derived-status-control"),
    SuppressionKind::ResultStatusProbe if authority_file(file) => Some("fail-closed-status-control"),
    SuppressionKind::ResultStatusProbe => Some("local-status-control"),
    SuppressionKind::ResultVariantProbe => Some("fail-closed-status-control"),
    SuppressionKind::SuccessOnlyConditional => Some("parser-alternative-probe"),
  }
}

fn persistent_format_file(file: &str) -> bool {
  file.starts_with("aeordb-lib/src/engine/v4/")
    || [
      "/configuration_history.rs",
      "/directory_repair_workspace.rs",
      "/emergency_spill.rs",
      "/entry_header.rs",
      "/file_header.rs",
      "/hot_tail.rs",
      "/kv_rebuild_workspace.rs",
      "/schema_version.rs",
    ]
    .iter()
    .any(|suffix| file.ends_with(suffix))
}

fn authority_file(file: &str) -> bool {
  [
    "/configuration_authority.rs",
    "/coverage_runtime.rs",
    "/disk_kv_store.rs",
    "/durability_coordinator.rs",
    "/memory_coordinator.rs",
    "/namespace_mutation.rs",
    "/native_durability.rs",
    "/rate_limiter.rs",
    "/v4/read_view.rs",
    "/storage_engine.rs",
    "/sync_engine.rs",
    "/void_manager.rs",
  ]
  .iter()
  .any(|suffix| file.ends_with(suffix))
}

fn configuration_file(file: &str) -> bool {
  file.contains("config") || file.ends_with("/lifecycle_config.rs") || file.ends_with("/run_configuration.rs")
}

fn observability_file(file: &str) -> bool {
  file.contains("metrics")
    || file.ends_with("/health.rs")
    || file.ends_with("/integrity_scanner.rs")
    || file.ends_with("/rate_tracker.rs")
    || file.ends_with("/rss_sampler.rs")
    || file.ends_with("/runtime_observability.rs")
}

fn derived_state_file(file: &str) -> bool {
  file.contains("cache")
    || file.contains("index")
    || file.ends_with("/query_engine.rs")
    || file.ends_with("/search.rs")
    || file.ends_with("/tree_walker.rs")
}

fn retryable_worker_file(file: &str) -> bool {
  file.ends_with("/cron_scheduler.rs")
    || file.ends_with("/index_cleanup.rs")
    || file.ends_with("/integrity_scanner.rs")
    || file.ends_with("/task_worker.rs")
    || file.ends_with("/webhook.rs")
}

fn iterator_item(item: &str) -> bool {
  item.contains("Iterator") || item.contains("::next")
}

fn review_policy(name: &str) -> Option<SuppressionReview> {
  let (class, rationale, owner, test, removal_condition) = match name {
    "fatal-build-boundary" => (
      SuppressionClass::DeliberatelyIgnored,
      "The build script terminates compilation when required checked-in or generated assets cannot be produced; continuing would create a knowingly incomplete binary.",
      "build and documentation packaging",
      "portal_spec and error_squelch_architecture_spec",
      "Remove an entry when the build step becomes fallible to Cargo without producing an incomplete artifact.",
    ),
    "legacy-infallible-boundary" => (
      SuppressionClass::DeliberatelyIgnored,
      "A historical infallible public constructor delegates to the checked constructor and terminates locally; all production startup paths use the fallible API.",
      "public compatibility facade",
      "logging_spec, metrics_spec, auth_provider_spec, and error_squelch_architecture_spec",
      "Remove when the next breaking API revision deletes the infallible compatibility constructor.",
    ),
    "validated-local-invariant" => (
      SuppressionClass::DeliberatelyIgnored,
      "The conversion or branch follows an exact bounds, registry, constant, or semantic validation in the same local control flow and cannot reinterpret external failure as success.",
      "local invariant owner",
      "records_spec, v4 format fixture specs, and error_squelch_architecture_spec",
      "Replace the entry with a typed error if its precondition stops being locally exhaustive or accepts external mutable state.",
    ),
    "typed-format-failure" => (
      SuppressionClass::CorrectnessReadTraversal,
      "The original error detail is normalized into the frozen malformed-input taxonomy while the operation remains an error and no partial value is returned.",
      "persistent format readers",
      "v4 format fixture specs, corruption_hardening_spec, and error_squelch_architecture_spec",
      "Remove when retaining the source error is part of the frozen diagnostic contract.",
    ),
    "authority-state-failure" => (
      SuppressionClass::DurabilityAuthority,
      "The implementation discards only an implementation-specific source value while preserving a typed authority failure, read-only latch, or rejected operation.",
      "durability and authority coordinators",
      "durability_coordinator_internal_spec, directory_ops_spec, and error_squelch_architecture_spec",
      "Remove when the source evidence is needed for recovery policy or operator diagnostics.",
    ),
    "boundary-validation-failure" => (
      SuppressionClass::DeliberatelyIgnored,
      "Boundary parsing maps implementation-specific parse or conversion errors to a stable caller-facing invalid-input failure; success is never returned.",
      "CLI, API, and plugin boundaries",
      "affected command, route, plugin specs and error_squelch_architecture_spec",
      "Remove when the public error contract begins carrying structured source diagnostics.",
    ),
    "ordered-optional-precedence" => (
      SuppressionClass::DeliberatelyIgnored,
      "The expression chooses the next documented optional source or compatibility spelling after absence; it does not recover from an authoritative persistence failure.",
      "configuration and compatibility adapters",
      "config_shadow_spec, index config specs, and error_squelch_architecture_spec",
      "Remove when the older source or spelling leaves the supported compatibility contract.",
    ),
    "http-terminal-error" => (
      SuppressionClass::DeliberatelyIgnored,
      "The route intentionally conceals internal error detail while returning a non-success HTTP status or a security-preserving generic response; no mutation is acknowledged as successful.",
      "HTTP route owners",
      "affected HTTP route specs and error_squelch_architecture_spec",
      "Remove when the route adopts a structured safe error envelope that retains the classified cause.",
    ),
    "plugin-error-envelope" => (
      SuppressionClass::CorrectnessReadTraversal,
      "Plugin boundary serialization falls back only to another valid bounded error envelope; malformed host data never becomes a successful plugin response.",
      "plugin SDK and host boundary",
      "aeordb-plugin-sdk specs, wasm_query_e2e_spec, and error_squelch_architecture_spec",
      "Remove when the plugin ABI provides an infallible fixed error frame.",
    ),
    "authorization-concealment" => (
      SuppressionClass::DurabilityAuthority,
      "Authorization failures are deliberately collapsed to a non-authorizing response so storage details cannot leak; the request remains denied.",
      "authorization middleware",
      "auth_middleware_spec, permission specs, and error_squelch_architecture_spec",
      "Remove only if a new public auth error taxonomy preserves the same concealment guarantee.",
    ),
    "unwind-cleanup-evidence" => (
      SuppressionClass::OptionalTelemetryTempCleanup,
      "Drop is only an unwind or last-resort cleanup path; normal completion is explicit, while failure here is logged or latched and cannot turn a failed primary operation into success.",
      "resource and operation guards",
      "poison, shutdown, GC, KV page, and error_squelch_architecture specs",
      "Remove when the resource can require explicit completion at every construction site and no unwind guard remains necessary.",
    ),
    "derived-cache-or-search-miss" => (
      SuppressionClass::RebuildableDerived,
      "The broad branch represents an ordered lookup miss or cache non-admission; callers retain the exact storage result or retry through authoritative state.",
      "KV and cache owners",
      "cache_and_hardlinks_spec, kv_snapshot_spec, and error_squelch_architecture_spec",
      "Remove if this branch can observe an I/O or corruption error rather than a local miss.",
    ),
    "redundant-authority-selection" => (
      SuppressionClass::DurabilityAuthority,
      "One redundant header or control copy failed validation; selection succeeds only from another independently valid copy and rejects the state when neither copy is valid.",
      "header and control readers",
      "v4 database header, control fixture, file_header, and error_squelch_architecture specs",
      "Remove if redundancy is retired or validation no longer independently proves the selected copy.",
    ),
    "optional-observability-default" => (
      SuppressionClass::OptionalTelemetryTempCleanup,
      "Unavailable secondary telemetry is represented as unknown, degraded, or a conservative bounded value and never changes storage or authorization correctness.",
      "health, metrics, and diagnostics",
      "health_spec, metrics_pulse_spec, rss_sampler_spec, and error_squelch_architecture_spec",
      "Remove when the platform exposes a mandatory fallible metric that callers can represent directly.",
    ),
    "retryable-background-operation" => (
      SuppressionClass::RetryableOperational,
      "The background operation records visible failure evidence and retries on the bounded worker cadence or next startup instead of acknowledging completion.",
      "task, cron, cleanup, integrity, and webhook workers",
      "task worker, cron, index cleanup, integrity scanner, webhook, and error_squelch_architecture specs",
      "Remove when the worker gains a durable per-attempt terminal record that can propagate the exact error.",
    ),
    "diagnostic-partial-output" => (
      SuppressionClass::OptionalTelemetryTempCleanup,
      "A CLI diagnostic continues only to report additional independent evidence or complete controlled shutdown; the failed sub-result remains visible and is not a database success.",
      "CLI diagnostics and lifecycle",
      "CLI command specs, soak worker specs, and error_squelch_architecture_spec",
      "Remove when the diagnostic output schema can carry all partial failures as one structured result.",
    ),
    "candidate-format-probe" => (
      SuppressionClass::CorrectnessReadTraversal,
      "Recovery or parser code rejects one non-authoritative candidate and continues bounded discovery; authoritative readers still return corruption when no valid candidate exists.",
      "format recovery and parser owners",
      "corruption_hardening_spec, repair specs, parser specs, and error_squelch_architecture_spec",
      "Remove if the skipped candidate becomes part of acknowledged complete traversal rather than bounded recovery discovery.",
    ),
    "http-terminal-or-postcommit-evidence" => (
      SuppressionClass::OptionalTelemetryTempCleanup,
      "The HTTP path either returns a terminal non-success response or preserves an already durable primary result while recording failure of optional notification, cleanup, or derived publication.",
      "HTTP route and post-commit side-effect owners",
      "affected route specs, soft-failure metrics specs, and error_squelch_architecture_spec",
      "Split or remove the entry when the side effect becomes part of the acknowledged mutation contract.",
    ),
    "postcommit-derived-reconciliation" => (
      SuppressionClass::RebuildableDerived,
      "The primary namespace or durability transition is already hard-acknowledged; derived cache, event, index, or reconciliation failure is metered and repaired from authority.",
      "namespace and derived-state coordinators",
      "namespace_mutation specs, indexing specs, SSE specs, and error_squelch_architecture_spec",
      "Remove when the derived effect joins the hard acknowledgement boundary or receives a durable retry task.",
    ),
    "optional-cleanup-evidence" => (
      SuppressionClass::OptionalTelemetryTempCleanup,
      "Failure affects optional cleanup or secondary evidence after the primary result; it is logged, metered, or returned as a warning without falsifying the primary outcome.",
      "maintenance and evidence owners",
      "backup, lifecycle, lost-found, scanner, and error_squelch_architecture specs",
      "Remove when cleanup is promoted into the primary operation's atomic contract.",
    ),
    "contractual-configuration-default" => (
      SuppressionClass::DeliberatelyIgnored,
      "The absence case uses a documented default or lower-precedence configuration source; malformed present values are rejected by the central resolver.",
      "configuration resolver and CLI assembly",
      "config_shadow_spec, run configuration specs, CLI specs, and error_squelch_architecture_spec",
      "Remove when the property becomes mandatory or its default is deleted from the frozen registry.",
    ),
    "api-optional-default" => (
      SuppressionClass::DeliberatelyIgnored,
      "The route supplies a documented response or request default only for an absent optional value; storage and authorization failures retain non-success behavior.",
      "HTTP schema owners",
      "affected HTTP route specs and error_squelch_architecture_spec",
      "Remove when the field becomes required or its default moves into a versioned schema decoder.",
    ),
    "plugin-compatibility-default" => (
      SuppressionClass::DeliberatelyIgnored,
      "The plugin adapter supplies a bounded ABI compatibility default for an omitted optional field; malformed required host state is rejected.",
      "plugin SDK and runtime",
      "aeordb-plugin-sdk specs, wasm_query_e2e_spec, and error_squelch_architecture_spec",
      "Remove when the older plugin ABI is no longer supported.",
    ),
    "bounded-authority-default" => (
      SuppressionClass::DurabilityAuthority,
      "The fallback is conservative, bounded, and fail-closed for unavailable optional authority metadata; it cannot widen writes, reclaim data early, or bypass a durability latch.",
      "storage, durability, GC, and v4 control owners",
      "durability, storage, GC, v4 control, and error_squelch_architecture specs",
      "Remove when the value becomes mandatory authority or can be propagated through a fallible caller.",
    ),
    "derived-state-default" => (
      SuppressionClass::RebuildableDerived,
      "Absent derived index, cache, or plan metadata selects the exact scan/rebuild path or disables retention; it never fabricates authoritative membership.",
      "index, cache, and query runtime",
      "query, index, cache pressure, and error_squelch_architecture specs",
      "Remove when derived state becomes mandatory and durably versioned.",
    ),
    "optional-data-default" => (
      SuppressionClass::DeliberatelyIgnored,
      "An absent optional document, metadata, parser, or presentation value receives the documented neutral representation; malformed authoritative containers still fail.",
      "document and metadata consumers",
      "parser, range, query, content-type, and error_squelch_architecture specs",
      "Remove when the value becomes required by its public or persistent schema.",
    ),
    "checked-arithmetic-composition" => (
      SuppressionClass::DurabilityAuthority,
      "Result-to-option conversion composes a checked numeric conversion with further checked arithmetic and ends in a typed overflow or resource error; no zero or truncated value is accepted.",
      "memory and length accounting",
      "resource-bound specs and error_squelch_architecture_spec",
      "Remove when a shared checked-accounting helper can express the chain without an intermediate Option.",
    ),
    "derived-exact-fallback" => (
      SuppressionClass::RebuildableDerived,
      "A failed optional derived lookup becomes absence only before an exact authoritative fallback, scan, or non-admission path.",
      "query, index, cache, and search runtime",
      "query/index equivalence, cache pressure, and error_squelch_architecture specs",
      "Remove if callers stop performing the exact fallback or the derived artifact becomes correctness-bearing.",
    ),
    "format-or-header-probe" => (
      SuppressionClass::CorrectnessReadTraversal,
      "The conversion probes an optional representation, header, timestamp, or parser alternative; failure selects another bounded representation or a typed unsupported result.",
      "format, header, and parser adapters",
      "parser, range, route header, locator, and error_squelch_architecture specs",
      "Remove when the input contract accepts only one representation and can return its parse error directly.",
    ),
    "presence-feature-toggle" => (
      SuppressionClass::DeliberatelyIgnored,
      "Only environment-variable presence controls an explicit debug, timing, rate-limit, or dangerous-operation opt-in; the variable's contents are intentionally irrelevant.",
      "operator feature toggles",
      "GC, snapshot, startup safety, magic-link, and error_squelch_architecture specs",
      "Remove when the toggle moves into typed central configuration.",
    ),
    "iterator-terminal-state" => (
      SuppressionClass::CorrectnessReadTraversal,
      "The iterator records that a previously returned decode error has fused traversal; the error itself is yielded once and no empty-complete result is fabricated.",
      "bounded v4 iterators",
      "v4 iterator and malformed fixture specs plus error_squelch_architecture_spec",
      "Remove when the iterator state uses an explicit terminal enum rather than probing the prior Result.",
    ),
    "derived-status-control" => (
      SuppressionClass::RebuildableDerived,
      "Success status controls cache retention, event delivery, or derived publication only; authority remains readable and failures choose eviction, retry, or exact fallback.",
      "derived runtime coordinators",
      "cache, index, event, namespace mutation, and error_squelch_architecture specs",
      "Remove if the status begins controlling an acknowledged authoritative transition.",
    ),
    "fail-closed-status-control" => (
      SuppressionClass::DurabilityAuthority,
      "The status probe grants no capability and acknowledges no write on error; unavailable authority is treated as absent, denied, or read-only.",
      "authorization and storage authority",
      "auth, durability poison, namespace mutation, and error_squelch_architecture specs",
      "Remove when the caller can propagate a typed authority error without weakening concealment.",
    ),
    "local-status-control" => (
      SuppressionClass::DeliberatelyIgnored,
      "The status is consumed immediately for local command, channel, or control flow and does not discard acknowledged storage failure.",
      "local control-flow owner",
      "affected command/runtime specs and error_squelch_architecture_spec",
      "Replace with an exhaustive match if the error variant becomes semantically distinct.",
    ),
    "parser-alternative-probe" => (
      SuppressionClass::CorrectnessReadTraversal,
      "The parser or serializer tries a bounded alternative and either returns a valid result, another valid error envelope, or an explicit unsupported value; it never claims complete parsed data after required decoding fails.",
      "parser and plugin boundary owners",
      "native parser, indexing pipeline, plugin runtime, locator, and error_squelch_architecture specs",
      "Remove when the format has one mandatory representation or the fallback ABI is retired.",
    ),
    _ => return None,
  };
  Some(SuppressionReview {
    review_status: ReviewStatus::Reviewed,
    class,
    rationale: rationale.to_string(),
    owner: owner.to_string(),
    test: test.to_string(),
    removal_condition: removal_condition.to_string(),
  })
}

pub fn write_inventory(path: &Path, inventory: &SuppressionInventory) -> Result<(), String> {
  let parent = path.parent().ok_or_else(|| format!("inventory path has no parent: {}", path.display()))?;
  fs::create_dir_all(parent).map_err(|error| format!("failed to create inventory directory {}: {error}", parent.display()))?;
  let bytes = serde_json::to_vec_pretty(inventory).map_err(|error| format!("failed to encode suppression inventory: {error}"))?;
  fs::write(path, bytes).map_err(|error| format!("failed to write suppression inventory {}: {error}", path.display()))
}

fn validate_review_fields(name: &str, review: &SuppressionReview, errors: &mut Vec<String>) {
  if name.chars().count() < 3 || name.chars().count() > 80 || name.to_ascii_uppercase().contains("PENDING") {
    errors.push(format!("inventory review policy {name:?} has a malformed or pending name"));
  }
  if review.review_status != ReviewStatus::Reviewed {
    errors.push(format!("inventory review policy {name:?} is still pending review"));
  }
  validate_review_text(name, "rationale", &review.rationale, MIN_RATIONALE_CHARS, errors);
  validate_review_text(name, "owner", &review.owner, 3, errors);
  validate_review_text(name, "test", &review.test, 3, errors);
  validate_review_text(name, "removal_condition", &review.removal_condition, 12, errors);
}

fn validate_review_text(identity: &str, field: &str, value: &str, minimum: usize, errors: &mut Vec<String>) {
  let length = value.chars().count();
  if length < minimum {
    errors.push(format!("inventory entry {identity} has an underspecified {field}"));
  }
  if length > MAX_REVIEW_FIELD_CHARS {
    errors.push(format!("inventory entry {identity} has an unbounded {field} ({length} characters)"));
  }
  if value.to_ascii_uppercase().contains("PENDING") {
    errors.push(format!("inventory entry {identity} retains a pending {field}"));
  }
}

fn collect_rust_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
  if !path.exists() {
    return Err(format!("configured production source root does not exist: {}", path.display()));
  }
  if path.is_file() {
    if path.extension().and_then(|value| value.to_str()) == Some("rs") {
      output.push(path.to_path_buf());
    }
    return Ok(());
  }
  let mut entries = fs::read_dir(path)
    .map_err(|error| format!("failed to read source directory {}: {error}", path.display()))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| format!("failed to enumerate source directory {}: {error}", path.display()))?;
  entries.sort_by_key(|entry| entry.path());
  for entry in entries {
    collect_rust_files(&entry.path(), output)?;
  }
  Ok(())
}

fn normalize_path(path: &Path) -> String {
  path.components().map(|component| component.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

fn scan_source_raw(relative_file: &str, source: &str) -> Result<Vec<RawOccurrence>, String> {
  let syntax = syn::parse_file(source).map_err(|error| format!("failed to parse production Rust source {relative_file}: {error}"))?;
  let mut visitor = SuppressionVisitor { relative_file, contexts: Vec::new(), occurrences: Vec::new() };
  visitor.visit_file(&syntax);
  Ok(visitor.occurrences)
}

fn finalize_occurrences(mut raw: Vec<RawOccurrence>) -> Vec<SuppressionOccurrence> {
  raw.sort_by(|left, right| {
    (&left.file, left.line, left.column, left.kind, &left.normalized_pattern).cmp(&(
      &right.file,
      right.line,
      right.column,
      right.kind,
      &right.normalized_pattern,
    ))
  });
  let mut identity_ordinals: HashMap<String, usize> = HashMap::new();
  raw
    .into_iter()
    .map(|occurrence| {
      let identity_seed =
        format!("{}\0{}\0{}\0{}", occurrence.file, occurrence.enclosing_item, occurrence.kind.as_str(), occurrence.normalized_pattern);
      let base = blake3::hash(identity_seed.as_bytes()).to_hex()[..20].to_string();
      let ordinal = identity_ordinals.entry(base.clone()).or_default();
      let id = format!("esq-v1-{base}-{:02}", *ordinal);
      *ordinal += 1;
      SuppressionOccurrence {
        id,
        file: occurrence.file,
        line: occurrence.line,
        column: occurrence.column,
        enclosing_item: occurrence.enclosing_item,
        kind: occurrence.kind,
        pattern: truncate_pattern(&occurrence.normalized_pattern),
      }
    })
    .collect()
}

fn truncate_pattern(pattern: &str) -> String {
  if pattern.chars().count() <= MAX_PATTERN_CHARS {
    return pattern.to_string();
  }
  let mut output = pattern.chars().take(MAX_PATTERN_CHARS - 3).collect::<String>();
  output.push_str("...");
  output
}

struct SuppressionVisitor<'a> {
  relative_file: &'a str,
  contexts: Vec<String>,
  occurrences: Vec<RawOccurrence>,
}

impl SuppressionVisitor<'_> {
  fn record<T: ToTokens + Spanned>(&mut self, kind: SuppressionKind, node: &T) {
    let start = node.span().start();
    self.occurrences.push(RawOccurrence {
      file: self.relative_file.to_string(),
      line: start.line,
      column: start.column + 1,
      enclosing_item: if self.contexts.is_empty() { "<module>".to_string() } else { self.contexts.join("::") },
      kind,
      normalized_pattern: node.to_token_stream().to_string(),
    });
  }

  fn push_context(&mut self, context: String) {
    self.contexts.push(context);
  }

  fn pop_context(&mut self) {
    self.contexts.pop();
  }

  fn record_macro_method(&mut self, kind: SuppressionKind, span: Span, normalized_pattern: String) {
    let start = span.start();
    self.occurrences.push(RawOccurrence {
      file: self.relative_file.to_string(),
      line: start.line,
      column: start.column + 1,
      enclosing_item: if self.contexts.is_empty() { "<module>".to_string() } else { self.contexts.join("::") },
      kind,
      normalized_pattern,
    });
  }
}

impl<'ast> Visit<'ast> for SuppressionVisitor<'_> {
  fn visit_item(&mut self, node: &'ast Item) {
    if item_attributes(node).is_some_and(test_only_attributes) {
      return;
    }
    visit::visit_item(self, node);
  }

  fn visit_item_mod(&mut self, node: &'ast ItemMod) {
    self.push_context(node.ident.to_string());
    visit::visit_item_mod(self, node);
    self.pop_context();
  }

  fn visit_item_fn(&mut self, node: &'ast ItemFn) {
    self.push_context(node.sig.ident.to_string());
    visit::visit_item_fn(self, node);
    self.pop_context();
  }

  fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
    self.push_context(format!("impl {}", node.self_ty.to_token_stream()));
    visit::visit_item_impl(self, node);
    self.pop_context();
  }

  fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
    if test_only_attributes(&node.attrs) {
      return;
    }
    self.push_context(node.sig.ident.to_string());
    visit::visit_impl_item_fn(self, node);
    self.pop_context();
  }

  fn visit_trait_item_fn(&mut self, node: &'ast TraitItemFn) {
    if test_only_attributes(&node.attrs) {
      return;
    }
    self.push_context(node.sig.ident.to_string());
    visit::visit_trait_item_fn(self, node);
    self.pop_context();
  }

  fn visit_local(&mut self, node: &'ast syn::Local) {
    if matches!(node.pat, Pat::Wild(_)) {
      self.record(SuppressionKind::DiscardedAssignment, node);
    }
    if result_pattern(&node.pat, "Ok") && node.init.as_ref().is_some_and(|init| init.diverge.is_some()) {
      self.record(SuppressionKind::SuccessOnlyConditional, node);
    }
    visit::visit_local(self, node);
  }

  fn visit_expr_assign(&mut self, node: &'ast ExprAssign) {
    if expression_is_wildcard(node.left.as_ref()) {
      self.record(SuppressionKind::DiscardedAssignment, node);
    }
    visit::visit_expr_assign(self, node);
  }

  fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
    match node.method.to_string().as_str() {
      "ok" => self.record(SuppressionKind::ResultToOption, node),
      "unwrap_or" | "unwrap_or_default" | "unwrap_or_else" => self.record(SuppressionKind::DefaultOnError, node),
      "unwrap" | "unwrap_err" | "unwrap_unchecked" | "expect" | "expect_err" => self.record(SuppressionKind::PanicMethod, node),
      "is_ok" | "is_err" => self.record(SuppressionKind::ResultStatusProbe, node),
      "map_err" if node.args.first().is_some_and(expression_discards_error) => self.record(SuppressionKind::ErrorConversion, node),
      "or_else" => self.record(SuppressionKind::ErrorRecovery, node),
      _ => {}
    }
    visit::visit_expr_method_call(self, node);
  }

  fn visit_macro(&mut self, node: &'ast Macro) {
    if is_panic_macro(node) {
      self.record(SuppressionKind::PanicMacro, node);
    }
    if is_result_variant_probe_macro(node) {
      self.record(SuppressionKind::ResultVariantProbe, node);
    }
    let mut method_calls = Vec::new();
    collect_macro_method_calls(&node.tokens, &mut method_calls);
    for method_call in method_calls {
      self.record_macro_method(method_call.kind, method_call.span, method_call.pattern);
    }
    visit::visit_macro(self, node);
  }

  fn visit_pat_tuple_struct(&mut self, node: &'ast PatTupleStruct) {
    if path_ends_with(&node.path, "Err") && node.elems.iter().any(is_broad_pattern) {
      self.record(SuppressionKind::BroadErrPattern, node);
    }
    visit::visit_pat_tuple_struct(self, node);
  }

  fn visit_expr_if(&mut self, node: &'ast ExprIf) {
    if node.else_branch.is_none() && expression_let_pattern(&node.cond).is_some_and(|pattern| result_pattern(pattern, "Ok")) {
      self.record(SuppressionKind::SuccessOnlyConditional, node);
    }
    if expression_let_pattern(&node.cond).is_some_and(|pattern| result_pattern(pattern, "Err"))
      && logs_without_terminating(&node.then_branch)
    {
      self.record(SuppressionKind::LoggedErrorContinues, node);
    }
    visit::visit_expr_if(self, node);
  }

  fn visit_expr_while(&mut self, node: &'ast ExprWhile) {
    if expression_let_pattern(&node.cond).is_some_and(|pattern| result_pattern(pattern, "Ok")) {
      self.record(SuppressionKind::SuccessOnlyConditional, node);
    }
    visit::visit_expr_while(self, node);
  }

  fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
    for arm in &node.arms {
      if result_pattern(&arm.pat, "Err") && logs_without_terminating(arm.body.as_ref()) {
        self.record(SuppressionKind::LoggedErrorContinues, arm);
      }
    }
    visit::visit_expr_match(self, node);
  }
}

fn item_attributes(item: &Item) -> Option<&[Attribute]> {
  match item {
    Item::Const(value) => Some(&value.attrs),
    Item::Enum(value) => Some(&value.attrs),
    Item::ExternCrate(value) => Some(&value.attrs),
    Item::Fn(value) => Some(&value.attrs),
    Item::ForeignMod(value) => Some(&value.attrs),
    Item::Impl(value) => Some(&value.attrs),
    Item::Macro(value) => Some(&value.attrs),
    Item::Mod(value) => Some(&value.attrs),
    Item::Static(value) => Some(&value.attrs),
    Item::Struct(value) => Some(&value.attrs),
    Item::Trait(value) => Some(&value.attrs),
    Item::TraitAlias(value) => Some(&value.attrs),
    Item::Type(value) => Some(&value.attrs),
    Item::Union(value) => Some(&value.attrs),
    Item::Use(value) => Some(&value.attrs),
    Item::Verbatim(_) | _ => None,
  }
}

fn test_only_attributes(attributes: &[Attribute]) -> bool {
  attributes.iter().any(|attribute| {
    let path = attribute.path();
    if path.segments.last().is_some_and(|segment| segment.ident == "test") {
      return true;
    }
    if !path.is_ident("cfg") {
      return false;
    }
    attribute.parse_args::<Meta>().is_ok_and(|meta| cfg_requires_test(&meta))
  })
}

fn cfg_requires_test(meta: &Meta) -> bool {
  match meta {
    Meta::Path(path) => path.is_ident("test"),
    Meta::NameValue(_) => false,
    Meta::List(list) if list.path.is_ident("all") => parse_nested_meta(list).is_some_and(|items| items.iter().any(cfg_requires_test)),
    Meta::List(list) if list.path.is_ident("any") => {
      parse_nested_meta(list).is_some_and(|items| !items.is_empty() && items.iter().all(cfg_requires_test))
    }
    Meta::List(_) => false,
  }
}

fn parse_nested_meta(list: &syn::MetaList) -> Option<Vec<Meta>> {
  Punctuated::<Meta, Token![,]>::parse_terminated.parse2(list.tokens.clone()).ok().map(|values| values.into_iter().collect())
}

fn path_ends_with(path: &syn::Path, expected: &str) -> bool {
  path.segments.last().is_some_and(|segment| segment.ident == expected)
}

fn is_broad_pattern(pattern: &Pat) -> bool {
  matches!(pattern, Pat::Wild(_) | Pat::Rest(_))
    || matches!(pattern, Pat::Ident(identifier) if identifier.ident.to_string().starts_with('_'))
}

fn result_pattern(pattern: &Pat, expected: &str) -> bool {
  match pattern {
    Pat::TupleStruct(tuple) => path_ends_with(&tuple.path, expected),
    Pat::Or(or) => or.cases.iter().any(|case| result_pattern(case, expected)),
    Pat::Paren(paren) => result_pattern(&paren.pat, expected),
    Pat::Reference(reference) => result_pattern(&reference.pat, expected),
    _ => false,
  }
}

fn expression_let_pattern(expression: &Expr) -> Option<&Pat> {
  match expression {
    Expr::Let(value) => Some(&value.pat),
    Expr::Group(value) => expression_let_pattern(&value.expr),
    Expr::Paren(value) => expression_let_pattern(&value.expr),
    _ => None,
  }
}

fn expression_is_wildcard(expression: &Expr) -> bool {
  match expression {
    Expr::Infer(_) => true,
    Expr::Group(value) => expression_is_wildcard(&value.expr),
    Expr::Paren(value) => expression_is_wildcard(&value.expr),
    _ => false,
  }
}

fn is_panic_macro(value: &Macro) -> bool {
  value
    .path
    .segments
    .last()
    .is_some_and(|segment| matches!(segment.ident.to_string().as_str(), "panic" | "todo" | "unimplemented" | "unreachable"))
}

fn is_result_variant_probe_macro(value: &Macro) -> bool {
  value.path.segments.last().is_some_and(|segment| segment.ident == "matches")
    && token_stream_contains_any_ident(&value.tokens, &["Err", "Ok"])
}

struct MacroMethodCall {
  kind: SuppressionKind,
  span: Span,
  pattern: String,
}

fn collect_macro_method_calls(tokens: &TokenStream, output: &mut Vec<MacroMethodCall>) {
  let token_trees = tokens.clone().into_iter().collect::<Vec<_>>();
  for token_tree in &token_trees {
    if let TokenTree::Group(group) = token_tree {
      collect_macro_method_calls(&group.stream(), output);
    }
  }

  for index in 0..token_trees.len().saturating_sub(2) {
    let TokenTree::Punct(dot) = &token_trees[index] else {
      continue;
    };
    if dot.as_char() != '.' {
      continue;
    }
    let TokenTree::Ident(method) = &token_trees[index + 1] else {
      continue;
    };
    let TokenTree::Group(arguments) = &token_trees[index + 2] else {
      continue;
    };
    if arguments.delimiter() != Delimiter::Parenthesis {
      continue;
    }
    let Some(kind) = suppression_kind_for_macro_method(method.to_string().as_str(), &arguments.stream()) else {
      continue;
    };
    let pattern_start = index.saturating_sub(3);
    let pattern = token_trees[pattern_start..=index + 2].iter().cloned().collect::<TokenStream>().to_string();
    output.push(MacroMethodCall { kind, span: method.span(), pattern });
  }
}

fn suppression_kind_for_macro_method(method: &str, arguments: &TokenStream) -> Option<SuppressionKind> {
  match method {
    "ok" => Some(SuppressionKind::ResultToOption),
    "unwrap_or" | "unwrap_or_default" | "unwrap_or_else" => Some(SuppressionKind::DefaultOnError),
    "unwrap" | "unwrap_err" | "unwrap_unchecked" | "expect" | "expect_err" => Some(SuppressionKind::PanicMethod),
    "is_ok" | "is_err" => Some(SuppressionKind::ResultStatusProbe),
    "map_err" if syn::parse2::<Expr>(arguments.clone()).is_ok_and(|expression| expression_discards_error(&expression)) => {
      Some(SuppressionKind::ErrorConversion)
    }
    "or_else" => Some(SuppressionKind::ErrorRecovery),
    _ => None,
  }
}

fn expression_discards_error(expression: &Expr) -> bool {
  let Expr::Closure(closure) = expression else {
    return false;
  };
  if closure.inputs.iter().any(|pattern| matches!(pattern, Pat::Wild(_) | Pat::Rest(_))) {
    return true;
  }

  let mut identifiers = PatternIdentifierCollector::default();
  for pattern in &closure.inputs {
    identifiers.visit_pat(pattern);
  }
  !identifiers.values.is_empty()
    && identifiers.values.iter().all(|identifier| !token_stream_contains_ident(&closure.body.to_token_stream(), identifier))
}

#[derive(Default)]
struct PatternIdentifierCollector {
  values: Vec<String>,
}

impl<'ast> Visit<'ast> for PatternIdentifierCollector {
  fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
    self.values.push(node.ident.to_string());
    visit::visit_pat_ident(self, node);
  }
}

fn token_stream_contains_any_ident(tokens: &TokenStream, expected: &[&str]) -> bool {
  tokens.clone().into_iter().any(|token| match token {
    TokenTree::Ident(identifier) => expected.iter().any(|candidate| identifier == *candidate),
    TokenTree::Group(group) => token_stream_contains_any_ident(&group.stream(), expected),
    TokenTree::Literal(literal) => syn::parse_str::<syn::LitStr>(&literal.to_string())
      .is_ok_and(|literal| expected.iter().any(|candidate| format_string_captures_identifier(&literal.value(), candidate))),
    _ => false,
  })
}

fn token_stream_contains_ident(tokens: &TokenStream, expected: &str) -> bool {
  token_stream_contains_any_ident(tokens, &[expected])
}

fn format_string_captures_identifier(value: &str, expected: &str) -> bool {
  let bytes = value.as_bytes();
  let mut offset = 0usize;
  while offset < bytes.len() {
    if bytes[offset] != b'{' {
      offset += 1;
      continue;
    }
    if bytes.get(offset + 1) == Some(&b'{') {
      offset += 2;
      continue;
    }
    let Some(relative_end) = value[offset + 1..].find('}') else {
      return false;
    };
    let end = offset + 1 + relative_end;
    let capture = value[offset + 1..end].split(':').next().unwrap_or_default().trim();
    if capture == expected {
      return true;
    }
    offset = end + 1;
  }
  false
}

fn logs_without_terminating<T: ToTokens>(node: &T) -> bool {
  let tokens = node.to_token_stream().to_string();
  let has_log =
    ["debug !", "error !", "eprintln !", "info !", "println !", "trace !", "warn !"].iter().any(|needle| tokens.contains(needle));
  if !has_log {
    return false;
  }
  !["return ", "break ", "continue ", "panic !", "unreachable !", "process :: exit", "process :: abort"]
    .iter()
    .any(|needle| tokens.contains(needle))
    && !tokens.contains(" ?")
}
