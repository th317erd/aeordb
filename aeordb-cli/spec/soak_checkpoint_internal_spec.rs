use super::prepare_checkpoint_append_with_barrier;
use aeordb::engine::native_durability::{NativeDurabilityError, NativeDurabilityOperation};
use std::fs::OpenOptions;

#[test]
fn append_admission_propagates_a_failed_barrier_after_truncation() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("barrier-failure.tsv");
  let prefix = b"/prior.json\t{}\n!\t/replaced.json\n";
  let mut original = prefix.to_vec();
  original.extend_from_slice(b"/incomplete");
  std::fs::write(&checkpoint, original).unwrap();
  let mut writer = OpenOptions::new().read(true).write(true).open(&checkpoint).unwrap();
  let mut barrier_called = false;
  let error = prepare_checkpoint_append_with_barrier(&mut writer, &checkpoint, |file| {
    barrier_called = true;
    assert_eq!(file.metadata().unwrap().len(), prefix.len() as u64, "the barrier must follow truncation");
    Err(NativeDurabilityError::from_io(
      NativeDurabilityOperation::DataBarrier,
      std::io::Error::other("injected checkpoint barrier failure"),
    ))
  })
  .unwrap_err();
  assert!(barrier_called, "the fixture must reach its injected barrier: {error}");
  assert!(
    error.contains("checkpoint tail truncation durability barrier failed") && error.contains("injected checkpoint barrier failure"),
    "{error}"
  );
  assert_eq!(std::fs::read(&checkpoint).unwrap(), prefix, "failure must not append a startup marker or fabricate a complete record");
}

#[test]
fn append_admission_does_not_mutate_complete_or_malformed_checkpoints() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("unchanged.tsv");
  for (contents, succeeds) in [(b"/prior.json\t{}\n".as_slice(), true), (b"malformed\n/incomplete", false)] {
    std::fs::write(&checkpoint, contents).unwrap();
    let mut writer = OpenOptions::new().read(true).write(true).open(&checkpoint).unwrap();
    let result =
      prepare_checkpoint_append_with_barrier(&mut writer, &checkpoint, |_file| panic!("no mutation means no truncation barrier"));
    assert_eq!(result.is_ok(), succeeds);
    assert_eq!(std::fs::read(&checkpoint).unwrap(), contents);
  }
}
