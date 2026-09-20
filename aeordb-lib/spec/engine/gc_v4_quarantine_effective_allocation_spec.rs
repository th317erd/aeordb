use super::*;
use aeordb::engine::v4::gc_quarantine::{CandidateDeltaRecordV1, CandidateDeltaRecordsV1, QuarantineClosureErrorV1};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn effective_quarantine_candidates_allocation_refusal_is_recoverable() {
  const CHILD_CASE: &str = "AEORDB_QUARANTINE_ALLOCATION_CASE";
  const NAME: &str = "effective::allocation::effective_quarantine_candidates_allocation_refusal_is_recoverable";
  if let Ok(case) = std::env::var(CHILD_CASE) {
    let (profile, target) = case.split_once(':').unwrap();
    let algorithm = match profile {
      "32" => HashAlgorithm::Blake3_256,
      "64" => HashAlgorithm::Sha512,
      _ => panic!("unknown allocation profile"),
    };
    let width = algorithm.hash_length();
    let (size, occurrence) = match target {
      "fence" => (24 + 2 * width, 1),
      "predecessor" => (width, 1),
      "effective-identity" => (24 + 2 * width, 2),
      "heap" => (std::mem::size_of::<usize>(), 1),
      "cursor" => (std::mem::size_of::<(CandidateDeltaRecordsV1<'_>, Option<CandidateDeltaRecordV1<'_>>)>(), 1),
      _ => panic!("unknown allocation target"),
    };
    let delta = fixture(&format!("agca-{}-candidate-delta-valid.bin", algorithm_name(algorithm)));
    with_effective_fixture(algorithm, &[delta], true, |request, _, _, _, memory| {
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let cancellation = CancellationToken::new();
      // Initialize the token's platform mutex before measuring GC buffers.
      // Its first lock allocates 64 bytes on macOS, the same size as SHA-512.
      assert!(!cancellation.is_cancelled());
      let action = || QuarantineEffectiveClosureV1::new(request, cancellation, memory);
      let (result, failure) =
        if occurrence == 1 { allocation_probe::measure(size, action) } else { allocation_probe::measure_nth(size, occurrence, action) };
      assert!(failure.injected_failure, "allocation target {case} must execute");
      let error = match result {
        Err(error) => error,
        Ok(_) => panic!("allocation refusal must not yield a closure"),
      };
      assert!(
        matches!(
          error,
          QuarantineEffectiveClosureErrorV1::Allocation(_)
            | QuarantineEffectiveClosureErrorV1::Closure(QuarantineClosureErrorV1::Allocation(_))
        ),
        "preserve typed allocation failure: {error}"
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    });
    return;
  }
  let executable = std::env::current_exe().unwrap();
  let mut failed = Vec::new();
  for width in [32, 64] {
    for target in ["fence", "predecessor", "effective-identity", "heap", "cursor"] {
      let case = format!("{width}:{target}");
      let mut child = Command::new(&executable)
        .args(["--exact", NAME, "--test-threads=1", "--nocapture"])
        .env(CHILD_CASE, &case)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
      let deadline = Instant::now() + Duration::from_secs(9);
      loop {
        if child.try_wait().unwrap().is_some() {
          break;
        }
        if Instant::now() >= deadline {
          child.kill().unwrap();
          child.wait().unwrap();
          panic!("allocation child exceeded its nine-second deadline: {case}");
        }
        std::thread::sleep(Duration::from_millis(10));
      }
      let output = child.wait_with_output().unwrap();
      assert!(output.stdout.len() + output.stderr.len() < 32768, "child output must remain bounded");
      if !output.status.success() {
        failed.push(format!(
          "{case}: {}; stdout={}; stderr={}",
          output.status,
          String::from_utf8_lossy(&output.stdout),
          String::from_utf8_lossy(&output.stderr)
        ));
      } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("test result: ok. 1 passed; 0 failed;"), "child must execute its exact regression: {case}: {stdout}");
      }
    }
  }
  assert!(failed.is_empty(), "allocation refusal must return an error without process termination: {}", failed.join("\n"));
}
