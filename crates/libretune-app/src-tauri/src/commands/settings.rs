//! Application settings commands.

use crate::{load_settings, save_settings, Settings};
use tauri::Emitter;

/// Get application settings
#[tauri::command]
pub async fn get_settings(app: tauri::AppHandle) -> Result<Settings, String> {
    Ok(load_settings(&app))
}

/// Update a single setting
#[tauri::command]
pub async fn update_setting(
    app: tauri::AppHandle,
    key: String,
    value: String,
) -> Result<(), String> {
    let mut settings = load_settings(&app);

    match key.as_str() {
        "units_system" => settings.units_system = value,
        "auto_burn_on_close" => {
            settings.auto_burn_on_close = value.parse().map_err(|_| "Invalid boolean value")?
        }
        "gauge_snap_to_grid" => {
            settings.gauge_snap_to_grid = value.parse().map_err(|_| "Invalid boolean value")?
        }
        "gauge_free_move" => {
            settings.gauge_free_move = value.parse().map_err(|_| "Invalid boolean value")?
        }
        "gauge_lock" => settings.gauge_lock = value.parse().map_err(|_| "Invalid boolean value")?,
        "auto_sync_gauge_ranges" => {
            settings.auto_sync_gauge_ranges = value.parse().map_err(|_| "Invalid boolean value")?
        }
        "indicator_column_count" => settings.indicator_column_count = value,
        "indicator_fill_empty" => {
            settings.indicator_fill_empty = value.parse().map_err(|_| "Invalid boolean value")?
        }
        "indicator_text_fit" => settings.indicator_text_fit = value,
        // Status bar channels (JSON array)
        "status_bar_channels" => {
            settings.status_bar_channels = serde_json::from_str(&value)
                .map_err(|e| format!("Invalid JSON for status_bar_channels: {}", e))?
        }
        // Heatmap scheme settings
        "heatmap_value_scheme" => settings.heatmap_value_scheme = value,
        "heatmap_change_scheme" => settings.heatmap_change_scheme = value,
        "heatmap_coverage_scheme" => settings.heatmap_coverage_scheme = value,
        // Help icon visibility
        "show_all_help_icons" => {
            settings.show_all_help_icons = value.parse().map_err(|_| "Invalid boolean value")?
        }
        // Alert rules settings
        "alert_large_change_enabled" => {
            settings.alert_large_change_enabled =
                value.parse().map_err(|_| "Invalid boolean value")?
        }
        "alert_large_change_abs" => {
            settings.alert_large_change_abs = value.parse().map_err(|_| "Invalid number value")?
        }
        "alert_large_change_percent" => {
            settings.alert_large_change_percent =
                value.parse().map_err(|_| "Invalid number value")?
        }
        "runtime_packet_mode" => {
            settings.runtime_packet_mode = value;
        }
        "last_serial_port" => {
            settings.last_serial_port = if value.is_empty() { None } else { Some(value) }
        }
        "auto_reconnect_after_controller_command" => {
            settings.auto_reconnect_after_controller_command =
                value.parse().map_err(|_| "Invalid boolean value")?
        }
        "auto_reconnect_after_firmware" => {
            settings.auto_reconnect_after_firmware =
                value.parse().map_err(|_| "Invalid boolean value")?
        }
        "onboarding_completed" => {
            settings.onboarding_completed = value.parse().map_err(|_| "Invalid boolean value")?
        }
        "table_y_axis_bottom" => {
            settings.table_y_axis_bottom = value.parse().map_err(|_| "Invalid boolean value")?
        }
        "table_cursor_color" => settings.table_cursor_color = value,
        "table_trail_color" => settings.table_trail_color = value,
        "table_trail_fade_sec" => {
            settings.table_trail_fade_sec = value.parse().map_err(|_| "Invalid number")?
        }
        // Session persistence
        "last_project_path" => {
            settings.last_project_path = if value.is_empty() { None } else { Some(value) }
        }
        "last_active_tab" => {
            settings.last_active_tab = if value.is_empty() { None } else { Some(value) }
        }
        // UI layout state.
        "sidebar_visible" => {
            settings.sidebar_visible = value.parse().map_err(|_| "Invalid boolean value")?
        }
        "agent_panel_visible" => {
            settings.agent_panel_visible = value.parse().map_err(|_| "Invalid boolean value")?
        }
        "selected_dashboard" => {
            settings.selected_dashboard = if value.is_empty() { None } else { Some(value) }
        }
        "open_tabs" => {
            // Store the JSON blob as-is; the frontend parses it.
            settings.open_tabs = if value.is_empty() { None } else { Some(value) }
        }
        "language" => settings.language = if value.is_empty() { None } else { Some(value) },

        // --- AI Assistant settings (all keys must have arms; see repo memory
        // about the latent auto_commit_on_save bug). ---
        "ai_assistant_enabled" => {
            // Refuse to enable without a risk acknowledgement.
            let want: bool = value.parse().map_err(|_| "Invalid boolean value")?;
            if want && !settings.ai_risk_acknowledged {
                return Err(
                    "Cannot enable AI assistant without acknowledging the risk warning"
                        .to_string(),
                );
            }
            settings.ai_assistant_enabled = want;
        }
        "ai_risk_acknowledged" => {
            settings.ai_risk_acknowledged =
                value.parse().map_err(|_| "Invalid boolean value")?
        }
        "ai_provider" => {
            // Changing the provider invalidates the prior risk ack boundary,
            // so force the user to re-acknowledge.
            if value != settings.ai_provider {
                settings.ai_risk_acknowledged = false;
                settings.ai_assistant_enabled = false;
            }
            settings.ai_provider = value;
        }
        "ai_base_url" => settings.ai_base_url = value,
        "ai_api_key" => {
            // Changing the key invalidates the prior risk ack boundary.
            if value != settings.ai_api_key {
                settings.ai_risk_acknowledged = false;
                settings.ai_assistant_enabled = false;
            }
            settings.ai_api_key = value;
        }
        "ai_model" => settings.ai_model = value,
        "ai_capability_tier" => settings.ai_capability_tier = value,
        _ => return Err(format!("Unknown setting: {}", key)),
    }

    save_settings(&app, &settings);
    let _ = app.emit("settings:changed", key.clone());
    Ok(())
}

/// Update custom heatmap color stops for a context
#[tauri::command]
pub async fn update_heatmap_custom_stops(
    app: tauri::AppHandle,
    context: String,
    stops: Vec<String>,
) -> Result<(), String> {
    let mut settings = load_settings(&app);

    match context.as_str() {
        "value" => settings.heatmap_value_custom = stops,
        "change" => settings.heatmap_change_custom = stops,
        "coverage" => settings.heatmap_coverage_custom = stops,
        _ => return Err(format!("Unknown heatmap context: {}", context)),
    }

    save_settings(&app, &settings);
    Ok(())
}
