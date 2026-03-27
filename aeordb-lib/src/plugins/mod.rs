pub mod native_runtime;
pub mod plugin_manager;
pub mod rule_engine;
pub mod scoping;
pub mod types;
pub mod wasm_runtime;

pub use native_runtime::NativePluginRuntime;
pub use plugin_manager::{PluginManager, PluginRecord};
pub use rule_engine::RuleEngine;
pub use scoping::{PluginPath, is_scope_accessible, parse_plugin_path, resolve_function_path};
pub use types::{PluginMetadata, PluginType, RuleContext, RuleDecision};
pub use wasm_runtime::WasmPluginRuntime;
