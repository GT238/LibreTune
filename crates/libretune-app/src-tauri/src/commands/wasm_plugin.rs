//! WASM plugin Tauri commands.
//!
//! Frontend interface to the sandboxed WebAssembly plugin runtime
//! (see `libretune_core::plugin_system`).

use crate::state::AppState;
use libretune_core::plugin_system::{
    Permission as WasmPermission, PluginConfig as WasmPluginConfig,
    PluginManager as WasmPluginManager, PluginManifest as WasmPluginManifest,
};
use serde::{Deserialize, Serialize};

/// Parse the frontend's string permission names (e.g. from consent-dialog
/// checkboxes) into typed [`WasmPermission`]s, ignoring anything unrecognized
/// rather than failing the whole load — an unrecognized name just can't be
/// granted, which is the safe default.
fn parse_permissions(names: &[String]) -> Vec<WasmPermission> {
    names
        .iter()
        .filter_map(|n| match n.as_str() {
            "ReadTables" => Some(WasmPermission::ReadTables),
            "WriteConstants" => Some(WasmPermission::WriteConstants),
            "SubscribeChannels" => Some(WasmPermission::SubscribeChannels),
            "ExecuteActions" => Some(WasmPermission::ExecuteActions),
            _ => None,
        })
        .collect()
}

/// Serializable plugin info returned to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmPluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub state: String,
    pub permissions: Vec<String>,
    pub exec_count: u64,
}

/// Ensure the WASM plugin manager is initialized.
fn new_plugin_manager() -> WasmPluginManager {
    WasmPluginManager::new(WasmPluginConfig {
        data_dir: String::new(),
        ecu_type: String::from("Unknown"),
        libretune_version: String::from(env!("CARGO_PKG_VERSION")),
    })
}

/// Load a WASM plugin from a .wasm file.
///
/// # Arguments
/// * `path` - Path to the .wasm plugin file
/// * `manifest_json` - JSON string with plugin manifest (the permissions the
///   plugin is *requesting*)
/// * `approved_permissions` - The permission names the user actually
///   approved (e.g. via a consent dialog listing the manifest's request).
///   Only permissions present in both this list and the manifest are
///   granted — a manifest can never self-grant a permission the user didn't
///   check.
///
/// Returns: Plugin name on success
#[tauri::command]
pub async fn load_wasm_plugin(
    path: String,
    manifest_json: String,
    approved_permissions: Vec<String>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let manifest: WasmPluginManifest = serde_json::from_str(&manifest_json)
        .map_err(|e| format!("Invalid plugin manifest: {}", e))?;

    let wasm_path = std::path::Path::new(&path);
    if !wasm_path.exists() {
        return Err(format!("WASM file not found: {}", path));
    }

    let approved = parse_permissions(&approved_permissions);

    let mut pm_guard = state.wasm_plugin_manager.lock().await;
    let pm = pm_guard.get_or_insert_with(new_plugin_manager);

    let name = pm.load_plugin(manifest, wasm_path, &approved)?;
    Ok(name)
}

/// Unload a WASM plugin by name.
#[tauri::command]
pub async fn unload_wasm_plugin(
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let mut pm_guard = state.wasm_plugin_manager.lock().await;
    let pm = pm_guard.as_mut().ok_or("Plugin manager not initialized")?;
    pm.unload_plugin(&name)
}

/// List all loaded WASM plugins with their info.
#[tauri::command]
pub async fn list_wasm_plugins(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<WasmPluginInfo>, String> {
    let pm_guard = state.wasm_plugin_manager.lock().await;

    match pm_guard.as_ref() {
        Some(pm) => {
            let list = pm.list_plugins();
            Ok(list
                .iter()
                .map(|(name, stats)| {
                    let (version, description, author, permissions) =
                        if let Some(plugin) = pm.get_plugin(name) {
                            let m = plugin.manifest();
                            (
                                m.version.clone(),
                                m.description.clone(),
                                m.author.clone(),
                                plugin
                                    .granted_permissions()
                                    .iter()
                                    .map(|p| format!("{:?}", p))
                                    .collect(),
                            )
                        } else {
                            (String::new(), String::new(), String::new(), vec![])
                        };

                    WasmPluginInfo {
                        name: name.clone(),
                        version,
                        description,
                        author,
                        state: format!("{:?}", stats.state),
                        permissions,
                        exec_count: stats.exec_count,
                    }
                })
                .collect())
        }
        None => Ok(vec![]),
    }
}

/// Execute a WASM plugin by name.
#[tauri::command]
pub async fn execute_wasm_plugin(
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<u64, String> {
    let mut pm_guard = state.wasm_plugin_manager.lock().await;
    let pm = pm_guard.as_mut().ok_or("Plugin manager not initialized")?;
    pm.execute_plugin(&name)
}

/// Get info about a specific WASM plugin.
#[tauri::command]
pub async fn get_wasm_plugin_info(
    name: String,
    state: tauri::State<'_, AppState>,
) -> Result<WasmPluginInfo, String> {
    let pm_guard = state.wasm_plugin_manager.lock().await;
    let pm = pm_guard.as_ref().ok_or("Plugin manager not initialized")?;

    let plugin = pm
        .get_plugin(&name)
        .ok_or_else(|| format!("Plugin '{}' not found", name))?;

    let stats = plugin.stats();
    let manifest = plugin.manifest();

    Ok(WasmPluginInfo {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        description: manifest.description.clone(),
        author: manifest.author.clone(),
        state: format!("{:?}", stats.state),
        permissions: plugin
            .granted_permissions()
            .iter()
            .map(|p| format!("{:?}", p))
            .collect(),
        exec_count: stats.exec_count,
    })
}
