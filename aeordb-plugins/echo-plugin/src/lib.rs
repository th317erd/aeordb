use aeordb_plugin_sdk::prelude::*;
use aeordb_plugin_sdk::aeordb_query_plugin;

aeordb_query_plugin!(echo_handle);

fn echo_handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
    let function = request
        .metadata
        .get("function_name")
        .map(|s| s.as_str())
        .unwrap_or("echo");

    match function {
        "echo" => {
            // Echo back the request metadata and body length
            PluginResponse::json(
                200,
                &serde_json::json!({
                    "echo": true,
                    "metadata": request.metadata,
                    "body_len": request.arguments.len(),
                }),
            )
            .map_err(|e| PluginError::SerializationFailed(e.to_string()))
        }
        "read" => {
            // Read a file by path (path passed in body)
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            match ctx.read_file(path) {
                Ok(file) => PluginResponse::json(
                    200,
                    &serde_json::json!({
                        "size": file.size,
                        "content_type": file.content_type,
                        "data_len": file.data.len(),
                    }),
                )
                .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(404, &e.to_string())),
            }
        }
        "extract" => {
            let payload: serde_json::Value = serde_json::from_slice(&request.arguments)
                .map_err(|e| PluginError::SerializationFailed(e.to_string()))?;
            let path = payload
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| PluginError::ExecutionFailed("missing path".to_string()))?;
            let extract_request = ExtractRequest {
                mode: payload
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("lines")
                    .to_string(),
                start: payload.get("start").and_then(|v| v.as_u64()),
                end: payload.get("end").and_then(|v| v.as_u64()),
                max_bytes: payload
                    .get("max_bytes")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize),
            };
            match ctx.extract_file(path, extract_request) {
                Ok(extracted) => PluginResponse::json(200, &serde_json::json!(extracted))
                    .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(500, &e.to_string())),
            }
        }
        "write" => {
            // Write a test file
            match ctx.write_file(
                "/plugin-output/result.json",
                b"{\"written\":true}",
                "application/json",
            ) {
                Ok(()) => PluginResponse::json(201, &serde_json::json!({"ok": true}))
                    .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(500, &e.to_string())),
            }
        }
        "delete" => {
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            match ctx.delete_file(path) {
                Ok(()) => PluginResponse::json(200, &serde_json::json!({"deleted": true}))
                    .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(500, &e.to_string())),
            }
        }
        "metadata" => {
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            match ctx.file_metadata(path) {
                Ok(meta) => PluginResponse::json(200, &serde_json::json!(meta))
                    .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(404, &e.to_string())),
            }
        }
        "list" => {
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            match ctx.list_directory(path) {
                Ok(entries) => {
                    PluginResponse::json(200, &serde_json::json!({"entries": entries}))
                        .map_err(|e| PluginError::SerializationFailed(e.to_string()))
                }
                Err(e) => Ok(PluginResponse::error(500, &e.to_string())),
            }
        }
        "status" => {
            // Return a custom status code
            Ok(PluginResponse::text(201, "Created by plugin"))
        }
        _ => Ok(PluginResponse::error(
            404,
            &format!("Unknown function: {}", function),
        )),
    }
}
