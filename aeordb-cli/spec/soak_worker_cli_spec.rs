use std::process::Command;

fn run(arguments: &[&str]) -> std::process::Output {
  Command::new(env!("CARGO_BIN_EXE_soak-worker")).args(arguments).output().expect("run soak-worker")
}

#[test]
fn malformed_numeric_arguments_are_rejected_instead_of_defaulted() {
  let output = run(&["--database", "unused.aeordb", "--source-dir", "unused", "--duration-hours", "not-a-number"]);

  assert_eq!(output.status.code(), Some(2));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("--duration-hours") && stderr.contains("number"), "unexpected stderr: {stderr}");
}

#[test]
fn unknown_arguments_are_rejected_instead_of_ignored() {
  let output = run(&["--duraton-hours", "1", "--summarize", "unused.tsv"]);

  assert_eq!(output.status.code(), Some(2));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("unknown argument") && stderr.contains("--duraton-hours"), "unexpected stderr: {stderr}");
}

#[test]
fn missing_flag_values_name_the_incomplete_argument() {
  let output = run(&["--duration-hours"]);

  assert_eq!(output.status.code(), Some(2));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("--duration-hours") && stderr.contains("requires a value"), "unexpected stderr: {stderr}");
}
