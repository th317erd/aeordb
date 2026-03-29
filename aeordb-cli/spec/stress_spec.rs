#[cfg(test)]
mod tests {
  use std::time::Duration;

  // Import the functions under test from the crate
  use aeordb_cli::commands::stress::{
    calculate_percentile, generate_random_data, parse_duration, parse_file_size,
  };

  // ─── parse_duration ───────────────────────────────────────────────

  #[test]
  fn test_parse_duration_seconds() {
    assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
  }

  #[test]
  fn test_parse_duration_minutes() {
    assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
  }

  #[test]
  fn test_parse_duration_hours() {
    assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
  }

  #[test]
  fn test_parse_duration_fractional_seconds() {
    assert_eq!(
      parse_duration("1.5s").unwrap(),
      Duration::from_millis(1500)
    );
  }

  #[test]
  fn test_parse_duration_fractional_minutes() {
    assert_eq!(
      parse_duration("0.5m").unwrap(),
      Duration::from_secs(30)
    );
  }

  #[test]
  fn test_parse_duration_uppercase_is_normalized() {
    assert_eq!(parse_duration("30S").unwrap(), Duration::from_secs(30));
    assert_eq!(parse_duration("5M").unwrap(), Duration::from_secs(300));
    assert_eq!(parse_duration("1H").unwrap(), Duration::from_secs(3600));
  }

  #[test]
  fn test_parse_duration_with_whitespace() {
    assert_eq!(parse_duration("  30s  ").unwrap(), Duration::from_secs(30));
  }

  #[test]
  fn test_parse_duration_empty_string() {
    let result = parse_duration("");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("empty"));
  }

  #[test]
  fn test_parse_duration_no_suffix() {
    let result = parse_duration("30");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("invalid duration format"));
  }

  #[test]
  fn test_parse_duration_invalid_number() {
    let result = parse_duration("abcs");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("invalid seconds value"));
  }

  #[test]
  fn test_parse_duration_zero_seconds() {
    let result = parse_duration("0s");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("positive"));
  }

  #[test]
  fn test_parse_duration_negative_seconds() {
    let result = parse_duration("-5s");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("positive"));
  }

  #[test]
  fn test_parse_duration_zero_minutes() {
    let result = parse_duration("0m");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("positive"));
  }

  #[test]
  fn test_parse_duration_zero_hours() {
    let result = parse_duration("0h");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("positive"));
  }

  #[test]
  fn test_parse_duration_unknown_suffix() {
    let result = parse_duration("30d");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("invalid duration format"));
  }

  // ─── parse_file_size ──────────────────────────────────────────────

  #[test]
  fn test_parse_file_size_bytes() {
    assert_eq!(parse_file_size("512b").unwrap(), 512);
  }

  #[test]
  fn test_parse_file_size_kilobytes() {
    assert_eq!(parse_file_size("1kb").unwrap(), 1024);
  }

  #[test]
  fn test_parse_file_size_megabytes() {
    assert_eq!(parse_file_size("1mb").unwrap(), 1_048_576);
  }

  #[test]
  fn test_parse_file_size_gigabytes() {
    assert_eq!(parse_file_size("1gb").unwrap(), 1_073_741_824);
  }

  #[test]
  fn test_parse_file_size_fractional_kilobytes() {
    assert_eq!(parse_file_size("1.5kb").unwrap(), 1536);
  }

  #[test]
  fn test_parse_file_size_fractional_megabytes() {
    assert_eq!(parse_file_size("0.5mb").unwrap(), 524_288);
  }

  #[test]
  fn test_parse_file_size_uppercase_is_normalized() {
    assert_eq!(parse_file_size("1KB").unwrap(), 1024);
    assert_eq!(parse_file_size("1MB").unwrap(), 1_048_576);
  }

  #[test]
  fn test_parse_file_size_with_whitespace() {
    assert_eq!(parse_file_size("  1kb  ").unwrap(), 1024);
  }

  #[test]
  fn test_parse_file_size_empty_string() {
    let result = parse_file_size("");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("empty"));
  }

  #[test]
  fn test_parse_file_size_no_suffix() {
    let result = parse_file_size("512");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("invalid file size format"));
  }

  #[test]
  fn test_parse_file_size_invalid_number() {
    let result = parse_file_size("abckb");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("invalid"));
  }

  #[test]
  fn test_parse_file_size_zero_bytes() {
    let result = parse_file_size("0b");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("positive"));
  }

  #[test]
  fn test_parse_file_size_zero_kilobytes() {
    let result = parse_file_size("0kb");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("positive"));
  }

  #[test]
  fn test_parse_file_size_negative_kilobytes() {
    let result = parse_file_size("-1kb");
    assert!(result.is_err());
  }

  #[test]
  fn test_parse_file_size_unknown_suffix() {
    let result = parse_file_size("1tb");
    assert!(result.is_err());
  }

  // ─── generate_random_data ─────────────────────────────────────────

  #[test]
  fn test_generate_random_data_correct_size() {
    assert_eq!(generate_random_data(0).len(), 0);
    assert_eq!(generate_random_data(1).len(), 1);
    assert_eq!(generate_random_data(1024).len(), 1024);
    assert_eq!(generate_random_data(65536).len(), 65536);
  }

  #[test]
  fn test_generate_random_data_not_all_zeros() {
    // With 1024 random bytes, it's astronomically unlikely they're all zeros
    let data = generate_random_data(1024);
    let has_nonzero = data.iter().any(|&byte| byte != 0);
    assert!(has_nonzero, "random data should contain non-zero bytes");
  }

  #[test]
  fn test_generate_random_data_different_each_call() {
    let first = generate_random_data(256);
    let second = generate_random_data(256);
    // Technically could be equal, but astronomically unlikely with 256 bytes
    assert_ne!(first, second, "two random buffers should differ");
  }

  // ─── calculate_percentile ─────────────────────────────────────────

  #[test]
  fn test_percentile_empty_returns_zero() {
    assert_eq!(calculate_percentile(&[], 0.5), Duration::ZERO);
  }

  #[test]
  fn test_percentile_single_element() {
    let latencies = vec![Duration::from_millis(42)];
    assert_eq!(
      calculate_percentile(&latencies, 0.5),
      Duration::from_millis(42)
    );
    assert_eq!(
      calculate_percentile(&latencies, 0.99),
      Duration::from_millis(42)
    );
  }

  #[test]
  fn test_percentile_p50_of_sorted_list() {
    let latencies: Vec<Duration> = (1..=100)
      .map(|milliseconds| Duration::from_millis(milliseconds))
      .collect();

    let p50 = calculate_percentile(&latencies, 0.5);
    assert_eq!(p50, Duration::from_millis(51)); // index 50 → value 51ms
  }

  #[test]
  fn test_percentile_p95_of_sorted_list() {
    let latencies: Vec<Duration> = (1..=100)
      .map(|milliseconds| Duration::from_millis(milliseconds))
      .collect();

    let p95 = calculate_percentile(&latencies, 0.95);
    assert_eq!(p95, Duration::from_millis(96)); // index 95 → value 96ms
  }

  #[test]
  fn test_percentile_p99_of_sorted_list() {
    let latencies: Vec<Duration> = (1..=100)
      .map(|milliseconds| Duration::from_millis(milliseconds))
      .collect();

    let p99 = calculate_percentile(&latencies, 0.99);
    assert_eq!(p99, Duration::from_millis(100)); // index 99 → value 100ms
  }

  #[test]
  fn test_percentile_p0_returns_first() {
    let latencies = vec![
      Duration::from_millis(10),
      Duration::from_millis(20),
      Duration::from_millis(30),
    ];
    assert_eq!(
      calculate_percentile(&latencies, 0.0),
      Duration::from_millis(10)
    );
  }

  #[test]
  fn test_percentile_two_elements() {
    let latencies = vec![Duration::from_millis(5), Duration::from_millis(100)];

    let p50 = calculate_percentile(&latencies, 0.5);
    assert_eq!(p50, Duration::from_millis(100)); // index 1

    let p95 = calculate_percentile(&latencies, 0.95);
    assert_eq!(p95, Duration::from_millis(100)); // index 1, clamped
  }
}
