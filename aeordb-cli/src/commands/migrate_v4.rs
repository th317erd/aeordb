use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aeordb::engine::config_resolver::CommandLineConfigOverrides;
use aeordb::engine::v4::migration_control::{MigrationPhaseV1, MigrationProgressStateV1};
use aeordb::engine::v4::migration_offline_run::{
  OfflineMigrationRunClockV1, OfflineMigrationRunIdentityV1, OfflineMigrationRunMilestoneObserverV1, OfflineMigrationRunMilestoneV1,
  OfflineMigrationRunReceiptV1, OfflineMigrationRunRequestV1, execute_offline_migration_v1,
};
use aeordb::engine::v4::migration_run_manifest::{MigrationRunBoundsV1, MigrationRunManifestV1, open_migration_run_manifest_v1};
use clap::Args;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const DEFAULT_LEASE_DURATION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Args, Debug)]
pub struct MigrateV4Args {
  /// Absolute canonical path to the read-only v3 source copy.
  #[arg(long, value_name = "PATH")]
  pub source: PathBuf,
  /// Absolute canonical path for the new, separate v4 destination.
  #[arg(long, value_name = "PATH")]
  pub destination: PathBuf,
  /// Absolute canonical path for the private durable migration workspace.
  #[arg(long, value_name = "PATH")]
  pub workspace: PathBuf,
  /// Exact 40-hex Git commit used to build this candidate; fresh runs only.
  #[arg(long, value_name = "40_HEX", required_unless_present = "resume", conflicts_with = "resume")]
  pub source_commit: Option<String>,
  /// Resume only the immutable run bound to --workspace.
  #[arg(long)]
  pub resume: bool,
  /// Emit NDJSON milestone events on stderr and one versioned JSON receipt on stdout.
  #[arg(long)]
  pub json: bool,
  /// Maximum time to wait for source maintenance authority.
  #[arg(long, default_value_t = 30, value_name = "SECONDS")]
  pub acquisition_timeout_seconds: u64,
  /// Migration algorithm memory bound.
  #[arg(long, value_parser = parse_byte_quantity, value_name = "BYTES")]
  pub maximum_memory_bytes: Option<u64>,
  /// Maximum bounded traversal/work items.
  #[arg(long, value_name = "COUNT")]
  pub maximum_work_items: Option<u64>,
  /// Maximum decoded entity/chunk size.
  #[arg(long, value_parser = parse_byte_quantity, value_name = "BYTES")]
  pub maximum_decoded_chunk_bytes: Option<u64>,
  /// Maximum namespace directory depth.
  #[arg(long, value_name = "COUNT")]
  pub maximum_directory_depth: Option<u32>,
  /// Maximum retained/current authority roots.
  #[arg(long, value_name = "COUNT")]
  pub maximum_authority_roots: Option<u64>,
  /// Maximum total authority records.
  #[arg(long, value_name = "COUNT")]
  pub maximum_authority_records: Option<u64>,
  /// Maximum durable root-map workspace bytes.
  #[arg(long, value_parser = parse_byte_quantity, value_name = "BYTES")]
  pub root_map_maximum_stored_bytes: Option<u64>,
  /// Maximum staged root-map rows.
  #[arg(long, value_name = "COUNT")]
  pub root_map_maximum_staged_rows: Option<u64>,
  /// Free bytes that must remain after root-map allocation.
  #[arg(long, value_parser = parse_byte_quantity, value_name = "BYTES")]
  pub root_map_minimum_free_bytes: Option<u64>,
  /// Maximum root-map external-sort memory.
  #[arg(long, value_parser = parse_byte_quantity, value_name = "BYTES")]
  pub root_map_maximum_sort_memory_bytes: Option<u64>,
  /// Maximum simultaneously open root-map merge runs.
  #[arg(long, value_name = "COUNT")]
  pub root_map_maximum_open_runs: Option<u32>,
  /// Maximum rows in one root-map page.
  #[arg(long, value_name = "COUNT")]
  pub root_map_maximum_page_rows: Option<u32>,
  /// Maximum root-map publication batch bytes.
  #[arg(long, value_parser = parse_byte_quantity, value_name = "BYTES")]
  pub root_map_maximum_publication_batch_bytes: Option<u64>,
  /// Maximum memory for prior-root lookup.
  #[arg(long, value_parser = parse_byte_quantity, value_name = "BYTES")]
  pub prior_lookup_maximum_memory_bytes: Option<u64>,
  /// Migration lease duration; fresh runs only.
  #[arg(long, value_name = "MILLISECONDS")]
  pub lease_duration_ms: Option<u64>,
}

impl MigrateV4Args {
  fn has_explicit_bounds(&self) -> bool {
    self.maximum_memory_bytes.is_some()
      || self.maximum_work_items.is_some()
      || self.maximum_decoded_chunk_bytes.is_some()
      || self.maximum_directory_depth.is_some()
      || self.maximum_authority_roots.is_some()
      || self.maximum_authority_records.is_some()
      || self.root_map_maximum_stored_bytes.is_some()
      || self.root_map_maximum_staged_rows.is_some()
      || self.root_map_minimum_free_bytes.is_some()
      || self.root_map_maximum_sort_memory_bytes.is_some()
      || self.root_map_maximum_open_runs.is_some()
      || self.root_map_maximum_page_rows.is_some()
      || self.root_map_maximum_publication_batch_bytes.is_some()
      || self.prior_lookup_maximum_memory_bytes.is_some()
      || self.lease_duration_ms.is_some()
  }

  fn fresh_bounds(&self) -> MigrationRunBoundsV1 {
    MigrationRunBoundsV1 {
      maximum_memory_bytes: self.maximum_memory_bytes.unwrap_or(512 * MIB),
      maximum_work_items: self.maximum_work_items.unwrap_or(1_000_000_000),
      maximum_decoded_chunk_bytes: self.maximum_decoded_chunk_bytes.unwrap_or(64 * MIB),
      maximum_directory_depth: self.maximum_directory_depth.unwrap_or(1_000),
      maximum_authority_roots: self.maximum_authority_roots.unwrap_or(1_000_000),
      maximum_authority_records: self.maximum_authority_records.unwrap_or(1_000_000),
      root_map_maximum_stored_bytes: self.root_map_maximum_stored_bytes.unwrap_or(4 * GIB),
      root_map_maximum_staged_rows: self.root_map_maximum_staged_rows.unwrap_or(1_000_000),
      root_map_minimum_free_bytes: self.root_map_minimum_free_bytes.unwrap_or(GIB),
      root_map_maximum_sort_memory_bytes: self.root_map_maximum_sort_memory_bytes.unwrap_or(256 * MIB),
      root_map_maximum_open_runs: self.root_map_maximum_open_runs.unwrap_or(32),
      root_map_maximum_page_rows: self.root_map_maximum_page_rows.unwrap_or(65_536),
      root_map_maximum_publication_batch_bytes: self.root_map_maximum_publication_batch_bytes.unwrap_or(16 * MIB),
      prior_lookup_maximum_memory_bytes: self.prior_lookup_maximum_memory_bytes.unwrap_or(256 * MIB),
      lease_duration_ms: self.lease_duration_ms.unwrap_or(DEFAULT_LEASE_DURATION_MS),
    }
  }
}

#[derive(Debug)]
pub struct MigrationV4CommandError {
  code: String,
  message: String,
}

impl MigrationV4CommandError {
  fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn json(&self) -> String {
    serde_json::to_string(&MigrationErrorOutput {
      protocol: "aeordb.offline-migration.error.v1",
      code: &self.code,
      message: &self.message,
    })
    .unwrap_or_else(|_| {
      "{\"protocol\":\"aeordb.offline-migration.error.v1\",\"code\":\"migration_cli_error_serialization\",\"message\":\"could not serialize migration error\"}".to_string()
    })
  }
}

impl Display for MigrationV4CommandError {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.message)
  }
}

impl std::error::Error for MigrationV4CommandError {}

#[derive(Serialize)]
struct MigrationErrorOutput<'a> {
  protocol: &'static str,
  code: &'a str,
  message: &'a str,
}

struct MigrationExecution {
  source: PathBuf,
  destination: PathBuf,
  workspace: PathBuf,
  executable: PathBuf,
  source_commit: [u8; 20],
  identity: OfflineMigrationRunIdentityV1,
  configuration_overrides: CommandLineConfigOverrides,
  bounds: MigrationRunBoundsV1,
  acquisition_timeout: Duration,
  clock: OfflineMigrationRunClockV1,
  cancellation: CancellationToken,
  resume: bool,
  json: bool,
}

struct MigrationMilestoneReporter {
  json: bool,
}

impl OfflineMigrationRunMilestoneObserverV1 for MigrationMilestoneReporter {
  fn should_pause_after(&mut self, milestone: OfflineMigrationRunMilestoneV1) -> bool {
    let milestone = milestone_name(milestone);
    if self.json {
      eprintln!(
        "{}",
        serde_json::json!({
          "protocol": "aeordb.offline-migration.progress.v1",
          "milestone": milestone,
        })
      );
    } else {
      eprintln!("Migration milestone: {milestone}");
    }
    false
  }
}

#[derive(Serialize)]
struct MigrationReceiptOutput {
  protocol: &'static str,
  resumed: bool,
  source: String,
  destination: String,
  workspace: String,
  manifest: String,
  identity: MigrationReceiptIdentity,
  binary: MigrationReceiptBinary,
  phase: &'static str,
  state: &'static str,
  destination_full_verified: bool,
  source_complete_file_checksum: String,
  destination_header_sequence: u64,
  copied_entity_count: u64,
  copied_content_bytes: u64,
  verified_root_count: u64,
  verified_entity_count: u64,
  verified_content_bytes: u64,
}

#[derive(Serialize)]
struct MigrationReceiptIdentity {
  database_id: String,
  migration_id: String,
  source_physical_instance_id: String,
  destination_physical_instance_id: String,
  holder_boot_id: String,
}

#[derive(Serialize)]
struct MigrationReceiptBinary {
  source_commit: String,
  executable_sha256: String,
}

pub async fn run(arguments: MigrateV4Args, configuration_overrides: CommandLineConfigOverrides) -> Result<(), MigrationV4CommandError> {
  let json = arguments.json;
  let cancellation = CancellationToken::new();
  let execution = prepare_execution(arguments, configuration_overrides, cancellation.clone())?;
  let mut worker = tokio::task::spawn_blocking(move || execute(execution));

  let result = tokio::select! {
    result = &mut worker => result.map_err(|error| MigrationV4CommandError::new("migration_cli_worker", error.to_string()))?,
    signal = migration_shutdown_signal() => {
      cancellation.cancel();
      let worker_result = worker.await.map_err(|error| MigrationV4CommandError::new("migration_cli_worker", error.to_string()))?;
      signal?;
      worker_result
    }
  }?;

  let encoded = serde_json::to_string(&result)
    .map_err(|error| MigrationV4CommandError::new("migration_cli_receipt_serialization", error.to_string()))?;
  if json {
    println!("{encoded}");
  } else {
    println!("AeorDB v3-to-v4 shadow migration verified");
    println!("Source: {}", result.source);
    println!("Destination: {}", result.destination);
    println!("Workspace: {}", result.workspace);
    println!("Source checksum: {}", result.source_complete_file_checksum);
    println!("Destination header sequence: {}", result.destination_header_sequence);
    println!("Verified entities: {}", result.verified_entity_count);
    println!("Machine receipt: {encoded}");
  }
  Ok(())
}

fn prepare_execution(
  arguments: MigrateV4Args,
  configuration_overrides: CommandLineConfigOverrides,
  cancellation: CancellationToken,
) -> Result<MigrationExecution, MigrationV4CommandError> {
  if arguments.acquisition_timeout_seconds == 0 {
    return Err(MigrationV4CommandError::new(
      "migration_cli_acquisition_timeout",
      "--acquisition-timeout-seconds must be greater than zero",
    ));
  }
  let executable = std::env::current_exe()
    .and_then(std::fs::canonicalize)
    .map_err(|error| MigrationV4CommandError::new("migration_cli_executable", error.to_string()))?;
  let wall_time_ms = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_err(|error| MigrationV4CommandError::new("migration_cli_clock", error.to_string()))?
    .as_millis()
    .try_into()
    .map_err(|error| MigrationV4CommandError::new("migration_cli_clock", format!("wall-clock milliseconds overflowed: {error}")))?;

  let (source_commit, identity, bounds) = if arguments.resume {
    if arguments.has_explicit_bounds() {
      return Err(MigrationV4CommandError::new(
        "migration_cli_resume_bounds",
        "resume reloads immutable run bounds; do not supply fresh-run bound options",
      ));
    }
    let manifest = open_migration_run_manifest_v1(&arguments.workspace, &cancellation)
      .map_err(|error| MigrationV4CommandError::new(error.code(), error.to_string()))?;
    (
      manifest.binary_source_commit(),
      OfflineMigrationRunIdentityV1 {
        database_id: manifest.database_id(),
        migration_id: manifest.migration_id(),
        source_physical_instance_id: manifest.source_physical_instance_id(),
        destination_physical_instance_id: manifest.destination_physical_instance_id(),
        holder_boot_id: manifest.holder_boot_id(),
      },
      manifest.bounds(),
    )
  } else {
    let source_commit = parse_source_commit(
      arguments
        .source_commit
        .as_deref()
        .ok_or_else(|| MigrationV4CommandError::new("migration_cli_source_commit", "fresh migration requires --source-commit"))?,
    )?;
    (
      source_commit,
      OfflineMigrationRunIdentityV1 {
        database_id: Uuid::new_v4().into_bytes(),
        migration_id: Uuid::new_v4().into_bytes(),
        source_physical_instance_id: Uuid::new_v4().into_bytes(),
        destination_physical_instance_id: Uuid::new_v4().into_bytes(),
        holder_boot_id: Uuid::new_v4().into_bytes(),
      },
      arguments.fresh_bounds(),
    )
  };

  Ok(MigrationExecution {
    source: arguments.source,
    destination: arguments.destination,
    workspace: arguments.workspace,
    executable,
    source_commit,
    identity,
    configuration_overrides,
    bounds,
    acquisition_timeout: Duration::from_secs(arguments.acquisition_timeout_seconds),
    clock: OfflineMigrationRunClockV1 { wall_time_ms, monotonic_time_ms: 1 },
    cancellation,
    resume: arguments.resume,
    json: arguments.json,
  })
}

fn execute(execution: MigrationExecution) -> Result<MigrationReceiptOutput, MigrationV4CommandError> {
  let mut reporter = MigrationMilestoneReporter { json: execution.json };
  let receipt = execute_offline_migration_v1(OfflineMigrationRunRequestV1 {
    source: &execution.source,
    destination: &execution.destination,
    workspace: &execution.workspace,
    executable: &execution.executable,
    source_commit: execution.source_commit,
    identity: execution.identity,
    configuration_overrides: execution.configuration_overrides.clone(),
    bounds: execution.bounds,
    acquisition_timeout: execution.acquisition_timeout,
    clock: execution.clock,
    cancellation: &execution.cancellation,
    resume: execution.resume,
    milestone_observer: Some(&mut reporter),
  })
  .map_err(|error| MigrationV4CommandError::new(error.code(), error.to_string()))?;
  let manifest = open_migration_run_manifest_v1(&execution.workspace, &execution.cancellation)
    .map_err(|error| MigrationV4CommandError::new(error.code(), error.to_string()))?;
  receipt_output(&execution, &manifest, receipt)
}

fn receipt_output(
  execution: &MigrationExecution,
  manifest: &MigrationRunManifestV1,
  receipt: OfflineMigrationRunReceiptV1,
) -> Result<MigrationReceiptOutput, MigrationV4CommandError> {
  let utf8 = |path: &std::path::Path| {
    path
      .to_str()
      .map(str::to_string)
      .ok_or_else(|| MigrationV4CommandError::new("migration_cli_path", "migration receipt path is not UTF-8"))
  };
  Ok(MigrationReceiptOutput {
    protocol: "aeordb.offline-migration.receipt.v1",
    resumed: execution.resume,
    source: utf8(&execution.source)?,
    destination: utf8(&execution.destination)?,
    workspace: utf8(&execution.workspace)?,
    manifest: utf8(manifest.path())?,
    identity: MigrationReceiptIdentity {
      database_id: hex::encode(execution.identity.database_id),
      migration_id: hex::encode(execution.identity.migration_id),
      source_physical_instance_id: hex::encode(execution.identity.source_physical_instance_id),
      destination_physical_instance_id: hex::encode(execution.identity.destination_physical_instance_id),
      holder_boot_id: hex::encode(execution.identity.holder_boot_id),
    },
    binary: MigrationReceiptBinary {
      source_commit: hex::encode(manifest.binary_source_commit()),
      executable_sha256: hex::encode(manifest.binary_executable_sha256()),
    },
    phase: phase_name(receipt.phase),
    state: state_name(receipt.state),
    destination_full_verified: receipt.destination_full_verified,
    source_complete_file_checksum: hex::encode(receipt.source_complete_file_checksum),
    destination_header_sequence: receipt.destination_header_sequence,
    copied_entity_count: receipt.copied_entity_count,
    copied_content_bytes: receipt.copied_content_bytes,
    verified_root_count: receipt.verified_root_count,
    verified_entity_count: receipt.verified_entity_count,
    verified_content_bytes: receipt.verified_content_bytes,
  })
}

fn parse_source_commit(value: &str) -> Result<[u8; 20], MigrationV4CommandError> {
  let decoded = hex::decode(value).map_err(|error| {
    MigrationV4CommandError::new(
      "migration_cli_source_commit",
      format!("--source-commit must be exactly 40 hexadecimal characters: {error}"),
    )
  })?;
  let commit: [u8; 20] = decoded.try_into().map_err(|_| {
    MigrationV4CommandError::new("migration_cli_source_commit", "--source-commit must be exactly 40 hexadecimal characters")
  })?;
  if commit == [0; 20] {
    return Err(MigrationV4CommandError::new("migration_cli_source_commit", "--source-commit must be nonzero"));
  }
  Ok(commit)
}

fn parse_byte_quantity(value: &str) -> Result<u64, String> {
  if value.is_empty() || value.starts_with('-') || value.starts_with('+') || value.contains('.') || value.chars().any(char::is_whitespace) {
    return Err(format!("invalid canonical byte quantity {value:?}"));
  }
  let (digits, multiplier) = [("KiB", 1024_u64), ("MiB", 1024_u64.pow(2)), ("GiB", 1024_u64.pow(3)), ("TiB", 1024_u64.pow(4))]
    .into_iter()
    .find_map(|(suffix, multiplier)| value.strip_suffix(suffix).map(|digits| (digits, multiplier)))
    .unwrap_or((value, 1));
  if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
    return Err(format!("invalid canonical byte quantity {value:?}"));
  }
  let number = digits.parse::<u64>().map_err(|_| format!("byte quantity {value:?} overflows u64"))?;
  number.checked_mul(multiplier).ok_or_else(|| format!("byte quantity {value:?} overflows u64"))
}

fn phase_name(phase: MigrationPhaseV1) -> &'static str {
  match phase {
    MigrationPhaseV1::Preflight => "preflight",
    MigrationPhaseV1::Copy => "copy",
    MigrationPhaseV1::Reconcile => "reconcile",
    MigrationPhaseV1::FinalFreeze => "final_freeze",
    MigrationPhaseV1::DestinationVerify => "destination_verify",
    MigrationPhaseV1::Cutover => "cutover",
    MigrationPhaseV1::ReadOnlyValidation => "read_only_validation",
    MigrationPhaseV1::OperatorAcceptance => "operator_acceptance",
  }
}

fn state_name(state: MigrationProgressStateV1) -> &'static str {
  match state {
    MigrationProgressStateV1::Pending => "pending",
    MigrationProgressStateV1::Running => "running",
    MigrationProgressStateV1::Paused => "paused",
    MigrationProgressStateV1::Complete => "complete",
    MigrationProgressStateV1::Failed => "failed",
    MigrationProgressStateV1::Canceled => "canceled",
  }
}

fn milestone_name(milestone: OfflineMigrationRunMilestoneV1) -> &'static str {
  match milestone {
    OfflineMigrationRunMilestoneV1::ManifestDurable => "manifest_durable",
    OfflineMigrationRunMilestoneV1::DestinationInitialized => "destination_initialized",
    OfflineMigrationRunMilestoneV1::MigrationControlsAcquired => "migration_controls_acquired",
    OfflineMigrationRunMilestoneV1::SourceGcSuspended => "source_gc_suspended",
    OfflineMigrationRunMilestoneV1::PreflightRunning => "preflight_running",
    OfflineMigrationRunMilestoneV1::PreflightComplete => "preflight_complete",
    OfflineMigrationRunMilestoneV1::CopyPending => "copy_pending",
    OfflineMigrationRunMilestoneV1::CopyRunning => "copy_running",
    OfflineMigrationRunMilestoneV1::BaseCloneStaged => "base_clone_staged",
    OfflineMigrationRunMilestoneV1::BaseSuccessorPublished => "base_successor_published",
    OfflineMigrationRunMilestoneV1::CopyComplete => "copy_complete",
    OfflineMigrationRunMilestoneV1::ReconcilePending => "reconcile_pending",
    OfflineMigrationRunMilestoneV1::ReconcileRunning => "reconcile_running",
    OfflineMigrationRunMilestoneV1::ReconcileComplete => "reconcile_complete",
    OfflineMigrationRunMilestoneV1::FinalFreezePending => "final_freeze_pending",
    OfflineMigrationRunMilestoneV1::FinalFreezeRunning => "final_freeze_running",
    OfflineMigrationRunMilestoneV1::FinalNamespaceReconciled => "final_namespace_reconciled",
    OfflineMigrationRunMilestoneV1::FinalAuthorityStaged => "final_authority_staged",
    OfflineMigrationRunMilestoneV1::FinalFreezeComplete => "final_freeze_complete",
    OfflineMigrationRunMilestoneV1::RootMapPublished => "root_map_published",
    OfflineMigrationRunMilestoneV1::DestinationVerificationPending => "destination_verification_pending",
    OfflineMigrationRunMilestoneV1::DestinationVerificationRunning => "destination_verification_running",
    OfflineMigrationRunMilestoneV1::DestinationVerificationComplete => "destination_verification_complete",
  }
}

async fn migration_shutdown_signal() -> Result<(), MigrationV4CommandError> {
  let ctrl_c = async {
    tokio::signal::ctrl_c()
      .await
      .map_err(|error| MigrationV4CommandError::new("migration_cli_signal", format!("failed to install or receive Ctrl+C: {error}")))
  };

  #[cfg(unix)]
  let terminate = async {
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
      .map_err(|error| MigrationV4CommandError::new("migration_cli_signal", format!("failed to install SIGTERM handler: {error}")))?;
    signal
      .recv()
      .await
      .ok_or_else(|| MigrationV4CommandError::new("migration_cli_signal", "SIGTERM listener closed before receiving a signal"))
  };

  #[cfg(not(unix))]
  let terminate = std::future::pending::<Result<(), MigrationV4CommandError>>();

  tokio::select! {
    result = ctrl_c => result,
    result = terminate => result,
  }
}

#[cfg(test)]
#[path = "../../spec/commands/migrate_v4_internal_spec.rs"]
mod migrate_v4_internal_spec;
