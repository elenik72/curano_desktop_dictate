use std::sync::Arc;

use tauri::State;

use super::manager::UploadsManager;
use super::types::UploadEntry;

#[tauri::command]
#[specta::specta]
pub fn uploads_list(manager: State<'_, Arc<UploadsManager>>) -> Vec<UploadEntry> {
    manager.entries()
}

#[tauri::command]
#[specta::specta]
pub fn uploads_add_files(
    manager: State<'_, Arc<UploadsManager>>,
    paths: Vec<String>,
) -> Result<(), String> {
    manager.add_files(paths)
}

#[tauri::command]
#[specta::specta]
pub fn uploads_cancel(manager: State<'_, Arc<UploadsManager>>, id: String) -> Result<(), String> {
    manager.cancel(&id)
}

#[tauri::command]
#[specta::specta]
pub fn uploads_retry(manager: State<'_, Arc<UploadsManager>>, id: String) -> Result<(), String> {
    manager.retry(&id)
}

#[tauri::command]
#[specta::specta]
pub async fn uploads_delete(
    manager: State<'_, Arc<UploadsManager>>,
    id: String,
) -> Result<(), String> {
    manager.delete(&id).await
}
