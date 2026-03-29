use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Args;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Args)]
pub struct StressArgs {
  /// Target server URL
  #[arg(short, long, default_value = "http://localhost:3000")]
  pub target: String,

  /// API key for authentication
  #[arg(short, long)]
  pub api_key: String,

  /// Number of concurrent workers
  #[arg(short, long, default_value = "10")]
  pub concurrency: usize,

  /// Test duration (e.g., "30s", "5m")
  #[arg(short, long, default_value = "10s")]
  pub duration: String,

  /// Operation type: write, read, mixed
  #[arg(short, long, default_value = "mixed")]
  pub operation: String,

  /// File size for write operations (e.g., "1kb", "1mb", "512b")
  #[arg(short, long, default_value = "1kb")]
  pub file_size: String,

  /// Path prefix for stress test files
  #[arg(short, long, default_value = "/stress-test")]
  pub path_prefix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationType {
  Write,
  Read,
  Mixed,
}

#[derive(Debug, Clone)]
struct OperationRecord {
  operation_type: OperationType,
  latency: Duration,
  success: bool,
}

#[derive(Debug)]
struct WorkerResult {
  records: Vec<OperationRecord>,
}

pub fn parse_duration(input: &str) -> Result<Duration, String> {
  let input = input.trim().to_lowercase();

  if input.is_empty() {
    return Err("duration string is empty".to_string());
  }

  if let Some(seconds_str) = input.strip_suffix('s') {
    let seconds: f64 = seconds_str
      .parse()
      .map_err(|_| format!("invalid seconds value: '{seconds_str}'"))?;
    if seconds <= 0.0 {
      return Err(format!("duration must be positive, got: {seconds}"));
    }
    return Ok(Duration::from_secs_f64(seconds));
  }

  if let Some(minutes_str) = input.strip_suffix('m') {
    let minutes: f64 = minutes_str
      .parse()
      .map_err(|_| format!("invalid minutes value: '{minutes_str}'"))?;
    if minutes <= 0.0 {
      return Err(format!("duration must be positive, got: {minutes}"));
    }
    return Ok(Duration::from_secs_f64(minutes * 60.0));
  }

  if let Some(hours_str) = input.strip_suffix('h') {
    let hours: f64 = hours_str
      .parse()
      .map_err(|_| format!("invalid hours value: '{hours_str}'"))?;
    if hours <= 0.0 {
      return Err(format!("duration must be positive, got: {hours}"));
    }
    return Ok(Duration::from_secs_f64(hours * 3600.0));
  }

  Err(format!(
    "invalid duration format: '{input}'. Expected a suffix of 's', 'm', or 'h' (e.g., '30s', '5m', '1h')"
  ))
}

pub fn parse_file_size(input: &str) -> Result<usize, String> {
  let input = input.trim().to_lowercase();

  if input.is_empty() {
    return Err("file size string is empty".to_string());
  }

  if let Some(megabytes_str) = input.strip_suffix("mb") {
    let megabytes: f64 = megabytes_str
      .parse()
      .map_err(|_| format!("invalid megabytes value: '{megabytes_str}'"))?;
    if megabytes <= 0.0 {
      return Err(format!("file size must be positive, got: {megabytes}mb"));
    }
    return Ok((megabytes * 1_048_576.0) as usize);
  }

  if let Some(kilobytes_str) = input.strip_suffix("kb") {
    let kilobytes: f64 = kilobytes_str
      .parse()
      .map_err(|_| format!("invalid kilobytes value: '{kilobytes_str}'"))?;
    if kilobytes <= 0.0 {
      return Err(format!("file size must be positive, got: {kilobytes}kb"));
    }
    return Ok((kilobytes * 1024.0) as usize);
  }

  if let Some(gigabytes_str) = input.strip_suffix("gb") {
    let gigabytes: f64 = gigabytes_str
      .parse()
      .map_err(|_| format!("invalid gigabytes value: '{gigabytes_str}'"))?;
    if gigabytes <= 0.0 {
      return Err(format!("file size must be positive, got: {gigabytes}gb"));
    }
    return Ok((gigabytes * 1_073_741_824.0) as usize);
  }

  if let Some(bytes_str) = input.strip_suffix('b') {
    let bytes: usize = bytes_str
      .parse()
      .map_err(|_| format!("invalid bytes value: '{bytes_str}'"))?;
    if bytes == 0 {
      return Err("file size must be positive".to_string());
    }
    return Ok(bytes);
  }

  Err(format!(
    "invalid file size format: '{input}'. Expected a suffix of 'b', 'kb', 'mb', or 'gb' (e.g., '512b', '1kb', '10mb')"
  ))
}

pub fn generate_random_data(size: usize) -> Vec<u8> {
  let mut random_generator = StdRng::from_entropy();
  let mut buffer = vec![0u8; size];
  random_generator.fill(&mut buffer[..]);
  buffer
}

pub fn calculate_percentile(sorted_latencies: &[Duration], percentile: f64) -> Duration {
  if sorted_latencies.is_empty() {
    return Duration::ZERO;
  }

  let index = ((sorted_latencies.len() as f64) * percentile) as usize;
  let clamped_index = index.min(sorted_latencies.len() - 1);
  sorted_latencies[clamped_index]
}

fn format_duration_millis(duration: Duration) -> String {
  let millis = duration.as_secs_f64() * 1000.0;
  format!("{millis:.1}ms")
}

fn format_file_size_human(bytes: usize) -> String {
  if bytes >= 1_073_741_824 {
    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
  } else if bytes >= 1_048_576 {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
  } else if bytes >= 1024 {
    format!("{:.1} KB", bytes as f64 / 1024.0)
  } else {
    format!("{bytes} B")
  }
}

fn parse_operation_type(input: &str) -> Result<OperationType, String> {
  match input.trim().to_lowercase().as_str() {
    "write" => Ok(OperationType::Write),
    "read" => Ok(OperationType::Read),
    "mixed" => Ok(OperationType::Mixed),
    other => Err(format!(
      "invalid operation type: '{other}'. Expected 'write', 'read', or 'mixed'"
    )),
  }
}

async fn authenticate(
  client: &reqwest::Client,
  target: &str,
  api_key: &str,
) -> Result<String, String> {
  let url = format!("{target}/auth/token");

  let response = client
    .post(&url)
    .json(&serde_json::json!({ "api_key": api_key }))
    .send()
    .await
    .map_err(|error| format!("authentication request failed: {error}"))?;

  if !response.status().is_success() {
    let status = response.status();
    let body = response
      .text()
      .await
      .unwrap_or_else(|_| "<unreadable body>".to_string());
    return Err(format!(
      "authentication failed with status {status}: {body}"
    ));
  }

  let body: serde_json::Value = response
    .json()
    .await
    .map_err(|error| format!("failed to parse authentication response: {error}"))?;

  body["token"]
    .as_str()
    .map(|token| token.to_string())
    .ok_or_else(|| "authentication response missing 'token' field".to_string())
}

async fn perform_write(
  client: &reqwest::Client,
  target: &str,
  token: &str,
  path: &str,
  data: &[u8],
) -> Result<(), String> {
  let url = format!("{target}/fs{path}");

  let response = client
    .put(&url)
    .bearer_auth(token)
    .body(data.to_vec())
    .send()
    .await
    .map_err(|error| format!("write request failed: {error}"))?;

  if !response.status().is_success() {
    let status = response.status();
    return Err(format!("write failed with status {status}"));
  }

  Ok(())
}

async fn perform_read(
  client: &reqwest::Client,
  target: &str,
  token: &str,
  path: &str,
) -> Result<(), String> {
  let url = format!("{target}/fs{path}");

  let response = client
    .get(&url)
    .bearer_auth(token)
    .send()
    .await
    .map_err(|error| format!("read request failed: {error}"))?;

  if !response.status().is_success() {
    let status = response.status();
    return Err(format!("read failed with status {status}"));
  }

  // Consume the body to ensure we measure full latency
  let _body = response
    .bytes()
    .await
    .map_err(|error| format!("failed to read response body: {error}"))?;

  Ok(())
}

struct WorkerConfiguration {
  client: reqwest::Client,
  target: String,
  token: String,
  path_prefix: String,
  operation_type: OperationType,
  file_size: usize,
  deadline: Instant,
  written_paths: Arc<RwLock<Vec<String>>>,
}

async fn run_worker(configuration: WorkerConfiguration) -> WorkerResult {
  let WorkerConfiguration {
    client,
    target,
    token,
    path_prefix,
    operation_type,
    file_size,
    deadline,
    written_paths,
  } = configuration;
  let mut records = Vec::new();
  let mut random_generator = StdRng::from_entropy();

  while Instant::now() < deadline {
    let should_write = match operation_type {
      OperationType::Write => true,
      OperationType::Read => false,
      OperationType::Mixed => random_generator.gen_bool(0.5),
    };

    if should_write {
      let file_path = format!("{path_prefix}/{}", Uuid::new_v4());
      let data = generate_random_data(file_size);
      let start = Instant::now();
      let result = perform_write(&client, &target, &token, &file_path, &data).await;
      let latency = start.elapsed();

      let success = result.is_ok();
      if success {
        written_paths.write().await.push(file_path);
      }

      records.push(OperationRecord {
        operation_type: OperationType::Write,
        latency,
        success,
      });
    } else {
      let maybe_path = {
        let paths = written_paths.read().await;
        if paths.is_empty() {
          None
        } else {
          let index = random_generator.gen_range(0..paths.len());
          Some(paths[index].clone())
        }
      };

      if let Some(read_path) = maybe_path {
        let start = Instant::now();
        let result = perform_read(&client, &target, &token, &read_path).await;
        let latency = start.elapsed();

        records.push(OperationRecord {
          operation_type: OperationType::Read,
          latency,
          success: result.is_ok(),
        });
      }
      // If no paths available yet, skip this iteration
    }
  }

  WorkerResult { records }
}

fn print_report(
  target: &str,
  actual_duration: Duration,
  concurrency: usize,
  operation_type: OperationType,
  file_size: usize,
  all_records: &[OperationRecord],
) {
  let total_operations = all_records.len();
  let error_count = all_records.iter().filter(|record| !record.success).count();
  let throughput = total_operations as f64 / actual_duration.as_secs_f64();

  let mut write_latencies: Vec<Duration> = all_records
    .iter()
    .filter(|record| record.operation_type == OperationType::Write && record.success)
    .map(|record| record.latency)
    .collect();
  write_latencies.sort();

  let mut read_latencies: Vec<Duration> = all_records
    .iter()
    .filter(|record| record.operation_type == OperationType::Read && record.success)
    .map(|record| record.latency)
    .collect();
  read_latencies.sort();

  let write_count = all_records
    .iter()
    .filter(|record| record.operation_type == OperationType::Write)
    .count();
  let read_count = all_records
    .iter()
    .filter(|record| record.operation_type == OperationType::Read)
    .count();

  let operation_label = match operation_type {
    OperationType::Write => "write",
    OperationType::Read => "read",
    OperationType::Mixed => "mixed",
  };

  let error_percentage = if total_operations > 0 {
    (error_count as f64 / total_operations as f64) * 100.0
  } else {
    0.0
  };

  let separator = "═".repeat(51);

  println!();
  println!("{separator}");
  println!("  AeorDB Stress Test Results");
  println!("{separator}");
  println!("  Target:       {target}");
  println!(
    "  Duration:     {:.1}s",
    actual_duration.as_secs_f64()
  );
  println!("  Concurrency:  {concurrency}");
  println!("  Operation:    {operation_label}");
  println!("  File Size:    {}", format_file_size_human(file_size));
  println!();
  println!("  Total Operations:   {total_operations}");
  println!("  Throughput:         {throughput:.1} ops/sec");

  if write_count > 0 {
    let write_throughput = write_count as f64 / actual_duration.as_secs_f64();
    println!();
    println!("  Write Operations:   {write_count}");
    println!("  Write Throughput:   {write_throughput:.1} ops/sec");
    if !write_latencies.is_empty() {
      println!("  Write Latency:");
      println!(
        "    p50:  {}",
        format_duration_millis(calculate_percentile(&write_latencies, 0.5))
      );
      println!(
        "    p95:  {}",
        format_duration_millis(calculate_percentile(&write_latencies, 0.95))
      );
      println!(
        "    p99:  {}",
        format_duration_millis(calculate_percentile(&write_latencies, 0.99))
      );
    }
  }

  if read_count > 0 {
    let read_throughput = read_count as f64 / actual_duration.as_secs_f64();
    println!();
    println!("  Read Operations:    {read_count}");
    println!("  Read Throughput:    {read_throughput:.1} ops/sec");
    if !read_latencies.is_empty() {
      println!("  Read Latency:");
      println!(
        "    p50:  {}",
        format_duration_millis(calculate_percentile(&read_latencies, 0.5))
      );
      println!(
        "    p95:  {}",
        format_duration_millis(calculate_percentile(&read_latencies, 0.95))
      );
      println!(
        "    p99:  {}",
        format_duration_millis(calculate_percentile(&read_latencies, 0.99))
      );
    }
  }

  println!();
  println!("  Errors:  {error_count} ({error_percentage:.2}%)");
  println!("{separator}");
}

pub async fn run(arguments: StressArgs) -> Result<(), String> {
  let duration = parse_duration(&arguments.duration)?;
  let file_size = parse_file_size(&arguments.file_size)?;
  let operation_type = parse_operation_type(&arguments.operation)?;

  if arguments.concurrency == 0 {
    return Err("concurrency must be at least 1".to_string());
  }

  println!("Authenticating with {}...", arguments.target);
  let client = reqwest::Client::new();
  let token = authenticate(&client, &arguments.target, &arguments.api_key).await?;
  println!("Authenticated successfully.");

  println!(
    "Starting stress test: {} workers, {:.1}s duration, {} operation, {} file size",
    arguments.concurrency,
    duration.as_secs_f64(),
    arguments.operation,
    format_file_size_human(file_size),
  );

  let written_paths: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(Vec::new()));
  let deadline = Instant::now() + duration;
  let test_start = Instant::now();

  let mut worker_handles = Vec::with_capacity(arguments.concurrency);

  for _ in 0..arguments.concurrency {
    let worker_client = client.clone();
    let worker_target = arguments.target.clone();
    let worker_token = token.clone();
    let worker_path_prefix = arguments.path_prefix.clone();
    let worker_written_paths = Arc::clone(&written_paths);

    let handle = tokio::spawn(run_worker(WorkerConfiguration {
      client: worker_client,
      target: worker_target,
      token: worker_token,
      path_prefix: worker_path_prefix,
      operation_type,
      file_size,
      deadline,
      written_paths: worker_written_paths,
    }));

    worker_handles.push(handle);
  }

  let mut all_records = Vec::new();
  for handle in worker_handles {
    match handle.await {
      Ok(result) => all_records.extend(result.records),
      Err(error) => {
        eprintln!("Worker task panicked: {error}");
      }
    }
  }

  let actual_duration = test_start.elapsed();

  print_report(
    &arguments.target,
    actual_duration,
    arguments.concurrency,
    operation_type,
    file_size,
    &all_records,
  );

  Ok(())
}
