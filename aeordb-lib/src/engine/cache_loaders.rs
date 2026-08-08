use uuid::Uuid;

use crate::auth::api_key::ApiKeyRecord;
use crate::engine::cache::CacheLoader;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::group::Group;
use crate::engine::index_config::PathIndexConfig;
use crate::engine::index_config_resolver::IndexConfigResolver;
use crate::engine::permissions::PathPermissions;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_store;
use crate::engine::user::User;

fn stored_authority_error(path: &str, error: EngineError) -> EngineError {
  match error {
    EngineError::JsonParseError(reason) => {
      EngineError::CorruptEntry { offset: 0, reason: format!("malformed stored authority at {path}: {reason}") }
    }
    other => other,
  }
}

/// Loads `.aeordb-permissions` files from directory paths.
pub struct PermissionsLoader;

impl CacheLoader for PermissionsLoader {
  type Key = String;
  type Value = Option<PathPermissions>;

  fn load(&self, path: &String, engine: &StorageEngine) -> EngineResult<Option<PathPermissions>> {
    let ops = DirectoryOps::new(engine);
    let permissions_path =
      if path == "/" || path.ends_with('/') { format!("{}.aeordb-permissions", path) } else { format!("{}/.aeordb-permissions", path) };

    match ops.read_file_buffered(&permissions_path) {
      Ok(data) => {
        let permissions = PathPermissions::deserialize_stored(&data, &permissions_path)?;
        Ok(Some(permissions))
      }
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(e) => Err(e),
    }
  }

  fn estimated_entry_bytes(&self, path: &String, value: &Option<PathPermissions>) -> u64 {
    let links = value.as_ref().map_or(0usize, |permissions| {
      permissions.links.iter().fold(0usize, |total, link| {
        total
          .saturating_add(std::mem::size_of_val(link))
          .saturating_add(link.group.capacity())
          .saturating_add(link.allow.capacity())
          .saturating_add(link.deny.capacity())
          .saturating_add(link.others_allow.as_ref().map_or(0, String::capacity))
          .saturating_add(link.others_deny.as_ref().map_or(0, String::capacity))
          .saturating_add(link.path_pattern.as_ref().map_or(0, String::capacity))
      })
    });
    std::mem::size_of::<(String, Option<PathPermissions>)>().saturating_add(path.capacity()).saturating_add(links) as u64
  }
}

/// Loads group memberships for a user by user_id.
pub struct GroupLoader;

impl CacheLoader for GroupLoader {
  type Key = Uuid;
  type Value = Vec<String>;

  fn load(&self, user_id: &Uuid, engine: &StorageEngine) -> EngineResult<Vec<String>> {
    let user_path = format!("/.aeordb-system/users/{user_id}");
    let user: User = match system_store::get_user(engine, user_id).map_err(|error| stored_authority_error(&user_path, error))? {
      Some(user) => user,
      None => return Ok(Vec::new()),
    };

    let all_groups: Vec<Group> =
      system_store::list_groups(engine).map_err(|error| stored_authority_error("/.aeordb-system/groups", error))?;

    let mut member_groups = Vec::new();
    for group in &all_groups {
      if group.evaluate_membership(&user) {
        member_groups.push(group.name.clone());
      }
    }

    Ok(member_groups)
  }

  fn estimated_entry_bytes(&self, _user_id: &Uuid, groups: &Vec<String>) -> u64 {
    std::mem::size_of::<(Uuid, Vec<String>)>()
      .saturating_add(groups.capacity().saturating_mul(std::mem::size_of::<String>()))
      .saturating_add(groups.iter().map(String::capacity).sum::<usize>()) as u64
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

    system_store::get_api_key(engine, key_uuid)
      .map_err(|error| stored_authority_error(&format!("/.aeordb-system/api-keys/{key_uuid}"), error))
  }

  fn estimated_entry_bytes(&self, key_id: &String, value: &Option<ApiKeyRecord>) -> u64 {
    let record_bytes = value.as_ref().map_or(0usize, |record| {
      std::mem::size_of_val(record)
        .saturating_add(record.key_hash.capacity())
        .saturating_add(record.label.as_ref().map_or(0, String::capacity))
        .saturating_add(record.rules.capacity().saturating_mul(std::mem::size_of::<crate::engine::api_key_rules::KeyRule>()))
        .saturating_add(record.rules.iter().map(|rule| rule.glob.capacity().saturating_add(rule.permitted.capacity())).sum::<usize>())
    });
    std::mem::size_of::<(String, Option<ApiKeyRecord>)>().saturating_add(key_id.capacity()).saturating_add(record_bytes) as u64
  }
}

/// Loads `.aeordb-config/indexes.json` from directory paths.
pub struct IndexConfigLoader;

impl CacheLoader for IndexConfigLoader {
  type Key = String;
  type Value = Option<PathIndexConfig>;

  fn load(&self, path: &String, engine: &StorageEngine) -> EngineResult<Option<PathIndexConfig>> {
    let ops = DirectoryOps::new(engine);
    let config_path = IndexConfigResolver::config_path_for_directory(path);

    match ops.read_file_buffered(&config_path) {
      Ok(data) => PathIndexConfig::deserialize(&data).map(Some).map_err(|error| stored_authority_error(&config_path, error)),
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(e) => Err(e),
    }
  }

  fn estimated_entry_bytes(&self, path: &String, value: &Option<PathIndexConfig>) -> u64 {
    let config_bytes = value.as_ref().map_or(0usize, |config| {
      std::mem::size_of_val(config)
        .saturating_add(config.parser.as_ref().map_or(0, String::capacity))
        .saturating_add(config.parser_memory_limit.as_ref().map_or(0, String::capacity))
        .saturating_add(config.glob.as_ref().map_or(0, String::capacity))
        .saturating_add(config.indexes.capacity().saturating_mul(std::mem::size_of::<crate::engine::index_config::IndexFieldConfig>()))
        .saturating_add(
          config
            .indexes
            .iter()
            .map(|index| {
              index
                .name
                .capacity()
                .saturating_add(index.index_type.capacity())
                .saturating_add(index.source.as_ref().map_or(0, estimated_json_bytes))
            })
            .sum::<usize>(),
        )
    });
    std::mem::size_of::<(String, Option<PathIndexConfig>)>().saturating_add(path.capacity()).saturating_add(config_bytes) as u64
  }
}

fn estimated_json_bytes(value: &serde_json::Value) -> usize {
  match value {
    serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => std::mem::size_of::<serde_json::Value>(),
    serde_json::Value::String(value) => std::mem::size_of::<serde_json::Value>().saturating_add(value.capacity()),
    serde_json::Value::Array(values) => values
      .capacity()
      .saturating_mul(std::mem::size_of::<serde_json::Value>())
      .saturating_add(values.iter().map(estimated_json_bytes).sum::<usize>()),
    serde_json::Value::Object(values) => values.iter().fold(
      std::mem::size_of_val(values).saturating_add(
        values
          .len()
          .saturating_mul(2)
          .saturating_mul(std::mem::size_of::<(String, serde_json::Value)>().saturating_add(2 * std::mem::size_of::<usize>())),
      ),
      |total, (key, value)| total.saturating_add(key.capacity()).saturating_add(estimated_json_bytes(value)),
    ),
  }
}
