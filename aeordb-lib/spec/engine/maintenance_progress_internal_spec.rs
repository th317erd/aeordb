use super::*;
use crate::engine::errors::EngineError;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Writer(Arc<Mutex<Vec<u8>>>);

impl Write for Writer {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    self.0.lock().unwrap().extend_from_slice(bytes);
    Ok(bytes.len())
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

fn capture(action: impl FnOnce()) -> Vec<serde_json::Value> {
  let output = Arc::new(Mutex::new(Vec::new()));
  let writer = output.clone();
  let subscriber = tracing_subscriber::fmt().json().without_time().with_writer(move || Writer(writer.clone())).finish();
  tracing::subscriber::with_default(subscriber, action);
  let bytes = output.lock().unwrap().clone();
  String::from_utf8(bytes).unwrap().lines().map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["fields"].clone()).collect()
}

#[test]
fn progress_is_periodic_bounded_saturating_and_does_not_claim_success_on_error() {
  let events = capture(|| {
    let error = MaintenanceProgress::run("test", "records", None, |progress| {
      for _ in 0..10000 {
        progress.advance_at(1, progress.started + Duration::from_secs(9));
      }
      progress.advance_at(7, progress.started + Duration::from_secs(10));
      progress.advance_at(u64::MAX, progress.started + Duration::from_secs(20));
      progress.advance_at(1, progress.started + Duration::from_secs(21));
      Err::<(), _>(EngineError::Cancelled("test cancellation".into()))
    })
    .unwrap_err();
    assert!(matches!(error, EngineError::Cancelled(message) if message == "test cancellation"));
  });
  assert_eq!(events.len(), 4);
  assert_eq!(events[0]["status"], "started");
  assert_eq!(events[1]["status"], "running");
  assert_eq!(events[1]["units"], 10007);
  assert_eq!(events[2]["units"], u64::MAX);
  assert_eq!(events[3]["status"], "interrupted");
  assert_eq!(events[3]["cache_state"], "absent");
}

#[test]
fn empty_success_reports_completion_and_unwinding_reports_interruption() {
  let events = capture(|| {
    assert_eq!(MaintenanceProgress::run("empty", "records", None, |_| Ok(17)).unwrap(), 17);
    let unwind = std::panic::catch_unwind(|| {
      MaintenanceProgress::run("panic", "records", None, |_| -> EngineResult<()> { panic!("test unwind") }).unwrap();
    });
    assert!(unwind.is_err());
  });
  assert_eq!(events.len(), 4);
  assert_eq!(events[1]["status"], "completed");
  assert_eq!(events[1]["units"], 0);
  assert_eq!(events[3]["status"], "interrupted");
}
