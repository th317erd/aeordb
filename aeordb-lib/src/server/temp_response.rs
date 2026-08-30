use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::engine::directory_ops::{reserve_streaming_read, ReservedReadChunk};
use crate::engine::{EngineResult, StorageEngine};

const RESPONSE_FRAME_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct ResponseBuildCancellation {
  token: CancellationToken,
}

impl ResponseBuildCancellation {
  pub(crate) fn check(&self) -> EngineResult<()> {
    if self.token.is_cancelled() {
      Err(crate::engine::EngineError::Cancelled("response construction was cancelled".to_string()))
    } else {
      Ok(())
    }
  }

  pub(crate) fn token(&self) -> &CancellationToken {
    &self.token
  }
}

pub(crate) struct ResponseBuildGuard {
  cancellation: ResponseBuildCancellation,
  armed: bool,
}

impl ResponseBuildGuard {
  pub(crate) fn new() -> Self {
    Self { cancellation: ResponseBuildCancellation { token: CancellationToken::new() }, armed: true }
  }

  pub(crate) fn cancellation(&self) -> ResponseBuildCancellation {
    self.cancellation.clone()
  }

  pub(crate) fn disarm(&mut self) {
    self.armed = false;
  }
}

impl Drop for ResponseBuildGuard {
  fn drop(&mut self) {
    if self.armed {
      self.cancellation.token.cancel();
    }
  }
}

pub(crate) fn tempfile_for_engine(engine: &StorageEngine, prefix: &str) -> std::io::Result<tempfile::NamedTempFile> {
  let directory =
    engine.database_path().parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
  tempfile::Builder::new().prefix(&format!(".aeordb-response-{prefix}-")).suffix(".tmp").tempfile_in(directory)
}

pub(crate) fn json_to_tempfile<T: serde::Serialize>(
  value: &T,
  engine: &StorageEngine,
  prefix: &str,
) -> EngineResult<tempfile::NamedTempFile> {
  let mut file = tempfile_for_engine(engine, prefix)?;
  serde_json::to_writer(file.as_file_mut(), value).map_err(|error| {
    if error.is_io() {
      crate::engine::EngineError::IoError(std::io::Error::other(error.to_string()))
    } else {
      crate::engine::EngineError::JsonParseError(error.to_string())
    }
  })?;
  Ok(file)
}

pub(crate) fn body_from_tempfile(mut file: tempfile::NamedTempFile, engine: Arc<StorageEngine>) -> EngineResult<(Body, u64)> {
  let content_length = file.as_file().metadata()?.len();
  file.as_file_mut().seek(SeekFrom::Start(0))?;
  let admitted_bytes = (RESPONSE_FRAME_BYTES as u64)
    .checked_add(std::mem::size_of::<ReservedReadChunk>() as u64)
    .ok_or_else(|| crate::engine::EngineError::ResourceExhausted("temporary response frame estimate overflow".to_string()))?;
  let initial_reservation = reserve_streaming_read(&engine, admitted_bytes, "temporary response stream admission failed")?;
  let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);

  tokio::task::spawn_blocking(move || {
    let mut initial_reservation = Some(initial_reservation);
    loop {
      let reservation = match initial_reservation.take() {
        Some(reservation) => reservation,
        None => match reserve_streaming_read(&engine, admitted_bytes, "temporary response frame admission failed") {
          Ok(reservation) => reservation,
          Err(error) => {
            if sender.blocking_send(Err(std::io::Error::other(error.to_string()))).is_err() {
              tracing::debug!("Temporary response receiver disconnected before an admission failure could be delivered");
            }
            break;
          }
        },
      };
      let mut data = vec![0; RESPONSE_FRAME_BYTES];
      let read = loop {
        match file.as_file_mut().read(&mut data) {
          Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
          result => break result,
        }
      };
      let read = match read {
        Ok(0) => break,
        Ok(read) => read,
        Err(error) => {
          if sender.blocking_send(Err(error)).is_err() {
            tracing::debug!("Temporary response receiver disconnected before a read failure could be delivered");
          }
          break;
        }
      };
      data.truncate(read);
      let chunk = ReservedReadChunk::from_admitted(data, reservation);
      if sender.blocking_send(Ok(Bytes::from_owner(chunk))).is_err() {
        break;
      }
    }
  });

  Ok((Body::from_stream(ReceiverStream::new(receiver)), content_length))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn response_build_guard_cancels_only_while_armed() {
    let guard = ResponseBuildGuard::new();
    let cancellation = guard.cancellation();
    assert!(cancellation.check().is_ok());
    drop(guard);
    assert!(matches!(cancellation.check(), Err(crate::engine::EngineError::Cancelled(_))));

    let mut guard = ResponseBuildGuard::new();
    let cancellation = guard.cancellation();
    guard.disarm();
    drop(guard);
    assert!(cancellation.check().is_ok());
  }
}
