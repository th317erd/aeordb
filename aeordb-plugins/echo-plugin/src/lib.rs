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
