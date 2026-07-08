use crate::devices::speechmike::{SpeechMikeManager, SpeechMikeStatus};
use crate::settings::{get_settings, write_settings};
use std::sync::Arc;
use tauri::{AppHandle, State};

#[tauri::command]
#[specta::specta]
pub fn get_speechmike_status(
    app: AppHandle,
    state: State<'_, Arc<SpeechMikeManager>>,
) -> SpeechMikeStatus {
    let mut status = state.get_status();
    if status.supported_platform {
        status.auto_select_enabled = get_settings(&app).speechmike_auto_select;
    }
    status
}

/// Processes with an active audio-capture session (Windows). Empty elsewhere.
/// Lets the UI warn that another app is also using the microphone.
#[tauri::command]
#[specta::specta]
pub async fn get_microphone_users() -> Result<Vec<String>, String> {
    // COM enumeration + tasklist can take a moment; keep it off the main thread.
    tauri::async_runtime::spawn_blocking(
        crate::devices::audio_sessions::list_capture_session_processes,
    )
    .await
    .map_err(|e| format!("microphone user scan failed: {e}"))
}

#[tauri::command]
#[specta::specta]
pub fn set_speechmike_auto_select(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = get_settings(&app);
    if enabled {
        // Toggling on: reset the user-override flag so auto-select resumes.
        settings.selected_microphone_user_overridden = false;
    }
    settings.speechmike_auto_select = enabled;
    write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn set_speechmike_button_mapping_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = get_settings(&app);
    settings.speechmike_button_mapping_enabled = enabled;
    write_settings(&app, settings);
    Ok(())
}
