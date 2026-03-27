use std::sync::Arc;

use crate::auth::JwtManager;
use crate::plugins::PluginManager;
use crate::storage::RedbStorage;

#[derive(Clone)]
pub struct AppState {
  pub storage: Arc<RedbStorage>,
  pub jwt_manager: Arc<JwtManager>,
  pub plugin_manager: Arc<PluginManager>,
}
