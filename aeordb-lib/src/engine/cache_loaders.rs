use uuid::Uuid;

use crate::auth::api_key::ApiKeyRecord;
use crate::engine::cache::CacheLoader;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::group::Group;
use crate::engine::index_config::PathIndexConfig;
use crate::engine::permissions::PathPermissions;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_store;
use crate::engine::user::User;

/// Loads `.aeordb-permissions` files from directory paths.
pub struct PermissionsLoader;

impl CacheLoader for PermissionsLoader {
    type Key = String;
    type Value = Option<PathPermissions>;

    fn load(&self, path: &String, engine: &StorageEngine) -> EngineResult<Option<PathPermissions>> {
        let ops = DirectoryOps::new(engine);
        let permissions_path = if path == "/" || path.ends_with('/') {
            format!("{}.aeordb-permissions", path)
        } else {
            format!("{}/.aeordb-permissions", path)
        };

        match ops.read_file(&permissions_path) {
            Ok(data) => {
                let permissions = PathPermissions::deserialize(&data)?;
                Ok(Some(permissions))
            }
            Err(EngineError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Loads group memberships for a user by user_id.
pub struct GroupLoader;

impl CacheLoader for GroupLoader {
    type Key = Uuid;
    type Value = Vec<String>;

    fn load(&self, user_id: &Uuid, engine: &StorageEngine) -> EngineResult<Vec<String>> {
        let user: User = match system_store::get_user(engine, user_id)? {
            Some(user) => user,
            None => return Ok(Vec::new()),
        };

        let all_groups: Vec<Group> = system_store::list_groups(engine)?;

        let mut member_groups = Vec::new();
        for group in &all_groups {
            if group.evaluate_membership(&user) {
                member_groups.push(group.name.clone());
            }
        }

        Ok(member_groups)
    }
}

/// Loads API key records by key_id string.
pub struct ApiKeyLoader;

impl CacheLoader for ApiKeyLoader {
    type Key = String;
    type Value = Option<ApiKeyRecord>;

    fn load(&self, key_id: &String, engine: &StorageEngine) -> EngineResult<Option<ApiKeyRecord>> {
        let key_uuid = match Uuid::parse_str(key_id) {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };

        let all_keys = system_store::list_api_keys(engine)?;
        Ok(all_keys.into_iter().find(|k| k.key_id == key_uuid))
    }
}

/// Loads `.aeordb-config/indexes.json` from directory paths.
pub struct IndexConfigLoader;

impl CacheLoader for IndexConfigLoader {
    type Key = String;
    type Value = Option<PathIndexConfig>;

    fn load(&self, path: &String, engine: &StorageEngine) -> EngineResult<Option<PathIndexConfig>> {
        let ops = DirectoryOps::new(engine);
        let config_path = if path.ends_with('/') {
            format!("{}.aeordb-config/indexes.json", path)
        } else {
            format!("{}/.aeordb-config/indexes.json", path)
        };

        match ops.read_file(&config_path) {
            Ok(data) => PathIndexConfig::deserialize(&data).map(Some),
            Err(EngineError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
