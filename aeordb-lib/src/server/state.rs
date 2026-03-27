use std::sync::Arc;

use crate::storage::RedbStorage;

#[derive(Clone)]
pub struct AppState {
  pub storage: Arc<RedbStorage>,
}
