use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::scalar_converter::{
  HashConverter, U8Converter, U16Converter, U32Converter, U64Converter,
  I64Converter, F64Converter, StringConverter, TimestampConverter,
  TrigramConverter, PhoneticConverter, ScalarConverter,
};

/// Configuration for a single indexed field.
#[derive(Debug, Clone)]
pub struct IndexFieldConfig {
  pub name: String,
  pub index_type: String,
  pub source: Option<serde_json::Value>,
  pub min: Option<f64>,
  pub max: Option<f64>,
}

/// Configuration for all indexes at a path.
#[derive(Debug, Clone)]
pub struct PathIndexConfig {
  pub indexes: Vec<IndexFieldConfig>,
  pub parser: Option<String>,
  pub parser_memory_limit: Option<String>,
  pub logging: bool,
}

impl PathIndexConfig {
  /// Serialize to JSON bytes.
  pub fn serialize(&self) -> Vec<u8> {
    let mut json = String::from("{");
    if let Some(ref parser) = self.parser {
      json.push_str(&format!("\"parser\":\"{}\",", parser));
    }
    if let Some(ref limit) = self.parser_memory_limit {
      json.push_str(&format!("\"parser_memory_limit\":\"{}\",", limit));
    }
    if self.logging {
      json.push_str("\"logging\":true,");
    }
    json.push_str("\"indexes\":[");
    for (position, config) in self.indexes.iter().enumerate() {
      if position > 0 {
        json.push(',');
      }
      json.push_str(&format!(
        "{{\"name\":\"{}\",\"type\":\"{}\"",
        config.name, config.index_type,
      ));
      if let Some(ref source) = config.source {
        json.push_str(&format!(",\"source\":{}", source));
      }
      if let Some(min) = config.min {
        json.push_str(&format!(",\"min\":{}", min));
      }
      if let Some(max) = config.max {
        json.push_str(&format!(",\"max\":{}", max));
      }
      json.push('}');
    }
    json.push_str("]}");
    json.into_bytes()
  }

  /// Deserialize from JSON bytes.
  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    let text = std::str::from_utf8(data).map_err(|error| {
      EngineError::JsonParseError(format!("Invalid UTF-8: {}", error))
    })?;

    // Use serde_json for parsing
    let parsed: serde_json::Value = serde_json::from_str(text).map_err(|error| {
      EngineError::JsonParseError(format!("Invalid JSON: {}", error))
    })?;

    let indexes_array = parsed.get("indexes")
      .and_then(|value| value.as_array())
      .ok_or_else(|| EngineError::JsonParseError("Missing 'indexes' array".to_string()))?;

    let parser = parsed.get("parser")
      .and_then(|v| v.as_str())
      .map(|v| v.to_string());

    let parser_memory_limit = parsed.get("parser_memory_limit")
      .and_then(|v| v.as_str())
      .map(|v| v.to_string());

    let logging = parsed.get("logging")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);

    let mut indexes = Vec::with_capacity(indexes_array.len());
    for item in indexes_array {
      let name = item.get("name")
        .or_else(|| item.get("field_name"))
        .or_else(|| item.get("field"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| EngineError::JsonParseError("Missing 'name' in index config".to_string()))?
        .to_string();

      let type_value = item.get("type")
        .or_else(|| item.get("converter_type"))
        .or_else(|| item.get("converter"))
        .ok_or_else(|| EngineError::JsonParseError("Missing 'type' in index config".to_string()))?;

      let source = item.get("source").cloned();
      let min = item.get("min").and_then(|value| value.as_f64());
      let max = item.get("max").and_then(|value| value.as_f64());

      // "type" can be a string or an array of strings
      let type_strings: Vec<String> = if let Some(s) = type_value.as_str() {
        vec![s.to_string()]
      } else if let Some(arr) = type_value.as_array() {
        arr.iter()
          .map(|v| v.as_str()
            .ok_or_else(|| EngineError::JsonParseError("'type' array must contain strings".to_string()))
            .map(|s| s.to_string()))
          .collect::<EngineResult<Vec<String>>>()?
      } else {
        return Err(EngineError::JsonParseError("'type' must be a string or array of strings".to_string()));
      };

      for index_type in type_strings {
        indexes.push(IndexFieldConfig {
          name: name.clone(),
          index_type,
          source: source.clone(),
          min,
          max,
        });
      }
    }

    Ok(PathIndexConfig { indexes, parser, parser_memory_limit, logging })
  }

  /// Deserialize JSON bytes and extract the optional "compression" field value.
  /// Returns Ok(Some("zstd")) if compression is configured, Ok(None) otherwise.
  pub fn deserialize_with_compression(data: &[u8]) -> EngineResult<Option<String>> {
    let text = std::str::from_utf8(data).map_err(|error| {
      EngineError::JsonParseError(format!("Invalid UTF-8: {}", error))
    })?;

    let parsed: serde_json::Value = serde_json::from_str(text).map_err(|error| {
      EngineError::JsonParseError(format!("Invalid JSON: {}", error))
    })?;

    let compression = parsed.get("compression")
      .and_then(|value| value.as_str())
      .map(|value| value.to_string());

    Ok(compression)
  }
}

/// Create a ScalarConverter from a config entry.
pub fn create_converter_from_config(config: &IndexFieldConfig) -> EngineResult<Box<dyn ScalarConverter>> {
  match config.index_type.as_str() {
    "hash" => Ok(Box::new(HashConverter)),
    "u8" => {
      let min = config.min.map(|v| v as u8).unwrap_or(0);
      let max = config.max.map(|v| v as u8).unwrap_or(u8::MAX);
      Ok(Box::new(U8Converter::with_range(min, max)))
    }
    "u16" => {
      let min = config.min.map(|v| v as u16).unwrap_or(0);
      let max = config.max.map(|v| v as u16).unwrap_or(u16::MAX);
      Ok(Box::new(U16Converter::with_range(min, max)))
    }
    "u32" => {
      let min = config.min.map(|v| v as u32).unwrap_or(0);
      let max = config.max.map(|v| v as u32).unwrap_or(u32::MAX);
      Ok(Box::new(U32Converter::with_range(min, max)))
    }
    "u64" => {
      let min = config.min.map(|v| v as u64).unwrap_or(0);
      let max = config.max.map(|v| v as u64).unwrap_or(u64::MAX);
      Ok(Box::new(U64Converter::with_range(min, max)))
    }
    "i64" => {
      let min = config.min.map(|v| v as i64).unwrap_or(i64::MIN);
      let max = config.max.map(|v| v as i64).unwrap_or(i64::MAX);
      Ok(Box::new(I64Converter::with_range(min, max)))
    }
    "f64" => {
      let min = config.min.unwrap_or(0.0);
      let max = config.max.unwrap_or(1.0);
      Ok(Box::new(F64Converter::with_range(min, max)))
    }
    "string" => {
      let max_length = config.max.map(|v| v as usize).unwrap_or(1024);
      Ok(Box::new(StringConverter::new(max_length)))
    }
    "timestamp" => {
      let min = config.min.map(|v| v as i64).unwrap_or(0);
      let max = config.max.map(|v| v as i64).unwrap_or(4_102_444_800_000);
      Ok(Box::new(TimestampConverter::with_range(min, max)))
    }
    "trigram" => Ok(Box::new(TrigramConverter)),
    "phonetic" | "dmetaphone" => Ok(Box::new(PhoneticConverter::dmetaphone())),
    "soundex" => Ok(Box::new(PhoneticConverter::soundex())),
    "dmetaphone_alt" => Ok(Box::new(PhoneticConverter::dmetaphone_alt())),
    unknown => Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("Unknown converter type: '{}'", unknown),
    }),
  }
}
